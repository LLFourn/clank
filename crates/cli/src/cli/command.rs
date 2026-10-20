//! The command line: the clap tree, its dispatch, and the README's
//! checkable claims. In the lib, not the bin, so those claims are
//! tested where every other test is — the bin has no test target of
//! its own (the-test-suite-takes-a-million-years).

use clap::builder::styling::{AnsiColor, Styles};
use clap::{CommandFactory, Parser, Subcommand};

/// Headings and command names stand out; clap drops the colour where
/// the terminal or `NO_COLOR` says to.
pub const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Yellow.on_default().bold())
    .usage(AnsiColor::Yellow.on_default().bold())
    .literal(AnsiColor::Cyan.on_default().bold())
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().bold())
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default());

/// How `clank --help` groups the top level. Names only: each row's
/// summary is the command's own about, so this table cannot disagree
/// with clap about what a command does.
pub const SECTIONS: &[(&str, &[&str])] = &[
    ("Getting set up", &["init", "setup", "doctor"]),
    (
        "The daily flow",
        &[
            "status",
            "tui",
            "log",
            "wait",
            "as",
            "auto",
            "run",
            "attending",
        ],
    ),
    (
        "Plans",
        &[
            "queue", "finish", "unfinish", "stash", "pick", "purge", "rereview", "diff", "html",
        ],
    ),
    ("Review", &["feedback", "pr-review", "block", "unblock"]),
    ("The team", &["agent", "team", "open", "fork"]),
    ("Talking", &["send", "nudge", "events"]),
    ("Remote", &["remote"]),
    ("Settings", &["config", "export"]),
    ("Plumbing", &["monitor", "rewire", "stop-hook"]),
];

/// Below this many columns beside the names, a summary reads better on
/// its own lines under its name than squeezed into a sliver.
const MIN_SUMMARY_ROOM: usize = 24;

/// The command line as `main` parses it: the derived tree with the
/// sectioned top-level help, wrapped to the terminal.
pub fn cli() -> clap::Command {
    let width = terminal_size::terminal_size()
        .map(|(w, _)| usize::from(w.0))
        .unwrap_or(80)
        .min(100);
    cli_at(width)
}

/// [`cli`] laid out for a terminal `width` columns wide.
pub fn cli_at(width: usize) -> clap::Command {
    let cmd = Cli::command().term_width(width);
    let header = STYLES.get_header();
    let sections = sections_help(&cmd, width);
    cmd.help_template(format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{sections}{header}Options:{header:#}\n{{options}}{{after-help}}"
    ))
}

/// The top level's command list, one block per [`SECTIONS`] entry. Each
/// summary wraps under its own column; when that column would leave too
/// little room, the summary goes under its name instead.
fn sections_help(cmd: &clap::Command, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let header = STYLES.get_header();
    let literal = STYLES.get_literal();
    let name_width = SECTIONS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|n| n.width())
        .max()
        .unwrap_or(0);
    let beside = 2 + name_width + 2;
    let stacked = width.saturating_sub(beside) < MIN_SUMMARY_ROOM;
    let column = if stacked { 4 } else { beside };
    let room = width.saturating_sub(column).max(1);
    let mut out = String::new();
    for (heading, names) in SECTIONS {
        out.push_str(&format!("{header}{heading}:{header:#}\n"));
        for name in *names {
            let about = cmd
                .find_subcommand(name)
                .and_then(|c| c.get_about())
                .map(|a| a.to_string())
                .unwrap_or_default();
            let mut lines = wrap(&about, room).into_iter();
            if stacked {
                out.push_str(&format!("  {literal}{name}{literal:#}\n"));
            } else {
                let pad = " ".repeat(name_width - name.width());
                out.push_str(&format!(
                    "  {literal}{name}{literal:#}{pad}  {}\n",
                    lines.next().unwrap_or_default()
                ));
            }
            for line in lines {
                out.push_str(&format!("{}{line}\n", " ".repeat(column)));
            }
        }
        out.push('\n');
    }
    out
}

/// Greedy word wrap by display width. A word wider than `room` gets a
/// line to itself rather than being cut.
fn wrap(text: &str, room: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > room {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

const INIT_EXAMPLES: &str = "\
Examples:
  # start using clank in the current repo
  clank init
  # and seed its roster from a saved team
  clank init --team review-pair";

const STATUS_EXAMPLES: &str = "\
Examples:
  clank status
  # keep re-printing as things change
  clank status --watch
  clank status -j";

const FINISH_EXAMPLES: &str = "\
Examples:
  # finish the one active plan: what changed, then why
  clank finish -m \"Parse errors name the line\" -m \"Users could not find the typo\"";

const SEND_EXAMPLES: &str = "\
Examples:
  clank send --master -m \"CI is red on main\"
  clank send --to codex --repo ../other -m \"Your review is waiting\"";

const STASH_EXAMPLES: &str = "\
Examples:
  # set the active plan aside, run another, then bring it back
  clank stash
  clank stash pop
  # give the plan up for now, keeping its body in the queue
  clank stash --to-queue";

#[derive(Parser)]
#[command(
    name = "clank",
    version,
    about = "Multi-agent peer review around plans",
    styles = STYLES
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
    #[command(after_long_help = INIT_EXAMPLES)]
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
    /// `-j` emits JSON; the full-screen pane is `clank tui`.
    #[command(after_long_help = STATUS_EXAMPLES)]
    Status(crate::cli::StatusArgs),
    /// Block until this agent has actionable work
    ///
    /// Author and role come from the session binding (`clank as`).
    /// `--peek` probes without blocking; `--for` observes a repo
    /// instead; `--event` adds ad-hoc wake sources (github repos,
    /// commands) alongside the agent config's `wait_events`.
    Wait(crate::cli::WaitCommandArgs),
    /// Manage the plan queue
    ///
    /// Plans wait here in priority order until the master starts them.
    /// A queued plan's body is written first as `.clank/drafts/<name>.md`.
    /// Removing an entry deletes it; unqueueing returns it to drafts.
    Queue(crate::cli::QueueArgs),
    /// Read or write review feedback
    ///
    /// Reviewers record one verdict per commit; the master reads them.
    /// Use this rather than reading the files under `.clank/agents/`.
    Feedback(crate::cli::FeedbackArgs),
    /// The event inbox: what your wake sources delivered
    ///
    /// Every event a wait ingests (GitHub activity, messages, scheduled
    /// events) lands here and keeps waking its agent until it is
    /// acknowledged. Listing and showing give metadata only. Reading
    /// returns the content, which is external and untrusted, and
    /// records that it was delivered to you.
    Events(crate::cli::EventsArgs),
    /// Put a message in an agent's inbox
    ///
    /// Addressed by repo (`--repo`, default the current one) and label
    /// (`--to`), or `--master` for whoever that repo's roster names
    /// master. The message wakes its agent until it is handled.
    #[command(after_long_help = SEND_EXAMPLES)]
    Send(crate::cli::SendArgs),
    /// Interrupt an agent and type a message into its terminal
    ///
    /// The escape hatch for when `clank send` cannot be delivered —
    /// a wedged hook, a stale binding, a wake that never fires. It
    /// reads no inbox and no session binding, and fails rather than
    /// guess which pane it meant. Unlike `send`, it leaves nothing
    /// waiting to be acknowledged; each attempt and its outcome is
    /// added to the agent's nudge history.
    Nudge(crate::cli::NudgeArgs),
    /// Watch a repo once, for every waiter
    ///
    /// One process per repo watches it and hands every waiting agent
    /// and `clank tui` what it is owed. Clank starts it when needed;
    /// you do not run it yourself.
    Monitor(crate::cli::MonitorArgs),
    /// Switch this repo's remote on or off through its `clank tui`.
    Remote(crate::cli::RemoteArgs),
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
    #[command(after_long_help = FINISH_EXAMPLES)]
    Finish(crate::cli::FinishArgs),
    /// Move a finalized plan back to active
    Unfinish(crate::cli::UnfinishArgs),

    // ── the roster & workspaces ──
    /// Manage this repo's agent roster
    ///
    /// Each agent on the roster has a tool, a launch profile and a
    /// role: master, a review tier, or standby.
    Agent(crate::cli::AgentArgs),
    /// Save and reuse roster templates across repos
    ///
    /// A team is a saved roster. Seed a new repo from one with
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
    /// With auto-mode on, the Stop hook keeps the session working: each
    /// time it ends a turn, clank wakes it with the next piece of work.
    /// Off, the session is driven by hand. Its role comes from the
    /// roster, not from here.
    Auto(crate::cli::AutoArgs),
    /// Open the team workspace in zellij
    ///
    /// Inside a live session, a new tab; outside one, attach to (or
    /// create) the repo's session.
    Open(crate::cli::OpenArgs),
    /// Fork the team into a linked worktree
    ///
    /// Creates the worktree with the whole team's sessions forked
    /// into it (opens a tab when inside zellij).
    Fork(crate::cli::ForkArgs),

    // ── plan surgery ──
    /// Set a plan's commits aside and restore them later
    ///
    /// Bare `clank stash [PLAN]` pushes, as `git stash` does: the one
    /// in-flight plan, or the named one. Its commits are kept on a
    /// protective ref and taken off the branch, so another plan can
    /// run. Where only one plan is stashed, commands that act on a
    /// stash take it without a name. `--to-queue` re-queues the plan
    /// body for a later attempt instead.
    #[command(after_long_help = STASH_EXAMPLES)]
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
    /// Questions only the human can answer
    ///
    /// An agent blocks on a decision it must not make itself; the human
    /// answers with `clank unblock`.
    Block(crate::cli::BlockArgs),
    /// Answer a pending block
    Unblock(crate::cli::UnblockArgs),
    /// Tell clank which background task you are waiting on
    ///
    /// The Stop hook then stops waking you with work you are already
    /// doing. Say how long you expect it to take (`--expect`, default 5m): if
    /// it is still running past that you are checked in on once, so a
    /// hang cannot hold you asleep. Nothing is cancelled.
    Attending(crate::cli::AttendingArgs),
    /// The full-screen pane for driving a running team
    ///
    /// The repo's state live: the agents, the plan and its gate, and the
    /// remote switch. One per repo. The `--tui` flag it grew out of still opens it,
    /// and says so.
    Tui(crate::cli::TuiArgs),
    /// Run a command under clank, so it knows exactly when the work ends
    ///
    /// Background this instead of the bare command:
    /// `clank run --desc "test run" -- cargo test`. Clank records what
    /// it is attending, then BECOMES the command — same pid, same
    /// stdio, same exit status — so the harness task's lifetime is the
    /// work's lifetime and nothing has to be told when it finished.
    /// `--expect 30m` says how long it should take; past that you are
    /// checked in on once, and nothing is cancelled.
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
    /// commit that every reviewer owes a fresh verdict on. Use it when a
    /// commit was accepted without the review it needed, such as a
    /// master's own commit continued automatically after it was
    /// demoted.
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
    /// Native Claude question hook adapter (installed by setup).
    #[command(hide = true)]
    QuestionHook,
}

/// Run the parsed command. The bin's `main` owns process concerns —
/// SIGPIPE, tracing, the exit code — and calls this for the rest.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    dispatch_with(cli, || std::io::read_to_string(std::io::stdin())).await
}

/// `dispatch` with the stdin `-m -` reads from supplied, so the path a
/// message actually travels can be driven in a test without starting a
/// binary.
pub async fn dispatch_with(
    cli: Cli,
    read_stdin: impl FnOnce() -> std::io::Result<String>,
) -> anyhow::Result<()> {
    let mut cli = cli;
    crate::cli::message::resolve_with(&mut cli.command, read_stdin)?;
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
        Command::Wait(args) => crate::cli::wait::run(args.wait).await,
        Command::Feedback(args) => crate::cli::feedback::run(args).await,
        Command::Events(args) => crate::cli::events::run(args).await,
        Command::Send(args) => crate::cli::send::run(args).await,
        Command::Nudge(args) => crate::cli::nudge::run(args).await,
        Command::Monitor(args) => crate::cli::monitor::run(args).await,
        Command::Remote(args) => crate::cli::remote_control::run(args).await,
        Command::As(args) => crate::cli::as_cmd::run(args).await,
        Command::Auto(args) => crate::cli::auto::run(args).await,
        Command::QuestionHook => crate::cli::web::claude_questions::run().await,
        Command::StopHook(args) => crate::cli::stop_hook::run(args).await,
        Command::Setup(args) => crate::cli::setup::run(args).await,
        Command::Doctor(args) => crate::cli::doctor::run(args).await,
        Command::Config(args) => crate::cli::config::run(args).await,
        Command::Block(args) => crate::cli::block::run(args).await,
        Command::Unblock(args) => crate::cli::block::run_unblock(args).await,
        Command::Attending(args) => crate::cli::attending::run(args).await,
        Command::Tui(args) => crate::cli::status_tui::run(args).await,
        Command::Run(args) => crate::cli::run::run(args),
    }
}

/// Map error variants to exit codes. An empty one-shot wait uses 3 so
/// the SessionStart probe can distinguish idle from a failed probe.
pub fn exit_code_for(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<crate::cli::wait::WaitNoWork>().is_some() {
        return 3;
    }
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

    /// The README's reference table rows, keyed by each command its first
    /// cell names (`block` / `unblock` share one), with the row's text.
    fn readme_rows(text: &str) -> Vec<(String, String)> {
        let reference = text
            .split("## Command reference")
            .nth(1)
            .expect("a Command reference section")
            .split("\n## ")
            .next()
            .unwrap();
        let mut rows = Vec::new();
        for line in reference.lines() {
            let Some(rest) = line.strip_prefix("| `") else {
                continue;
            };
            let Some(cell) = rest.split(" |").next() else {
                continue;
            };
            for name in cell.split(" / ") {
                rows.push((name.trim().trim_matches('`').to_string(), line.to_string()));
            }
        }
        rows
    }

    /// Whether `row` names `child` of `parent` in code: `child`,
    /// `parent child`, or `clank parent child`, as the first word of a
    /// backticked span.
    fn row_names_child(row: &str, parent: &str, child: &str) -> bool {
        row.split('`').skip(1).step_by(2).any(|span| {
            let mut words = span.split_whitespace().peekable();
            if words.peek() == Some(&"clank") {
                words.next();
            }
            if words.peek() == Some(&parent) {
                words.next();
            }
            words.next() == Some(child)
        })
    }

    /// Every visible command has a row in the README's reference table,
    /// and every visible subcommand is named in its own parent's row.
    /// Hidden commands and aliases are left out on purpose.
    fn readme_gaps(text: &str) -> Vec<String> {
        use clap::CommandFactory;
        let rows = readme_rows(text);
        let mut gaps = Vec::new();
        let cmd = Cli::command();
        for top in visible_children(&cmd) {
            let name = top.get_name();
            let Some((_, row)) = rows.iter().find(|(n, _)| n == name) else {
                gaps.push(format!("no row for `{name}`"));
                continue;
            };
            for child in visible_children(top) {
                if !row_names_child(row, name, child.get_name()) {
                    gaps.push(format!(
                        "`{name}`'s row does not name `{}`",
                        child.get_name()
                    ));
                }
            }
        }
        gaps
    }

    #[test]
    fn the_readme_reference_covers_every_visible_command() {
        let gaps = readme_gaps(&readme());
        assert!(
            gaps.is_empty(),
            "the README reference is missing:\n{}",
            gaps.join("\n")
        );
    }

    /// The check fails on what it exists to catch: a dropped row, a
    /// dropped child, and a child named only in someone else's row.
    #[test]
    fn readme_gaps_catches_a_missing_row_and_a_missing_child() {
        let text = readme();
        let row_of = |name: &str| {
            text.lines()
                .find(|l| l.starts_with(&format!("| `{name}` |")))
                .unwrap()
                .to_string()
        };
        let without_remote = text.replace(&format!("{}\n", row_of("remote")), "");
        assert!(readme_gaps(&without_remote).contains(&"no row for `remote`".to_string()));
        let queue = row_of("queue");
        let without_unqueue = text.replace(&queue, &queue.replace("`unqueue`", "unqueue"));
        assert!(
            readme_gaps(&without_unqueue)
                .contains(&"`queue`'s row does not name `unqueue`".to_string())
        );
        let moved =
            without_unqueue.replace(&row_of("init"), &format!("{} `unqueue`", row_of("init")));
        assert!(
            readme_gaps(&moved).contains(&"`queue`'s row does not name `unqueue`".to_string()),
            "a mention in another row does not count"
        );
        let block = row_of("block` / `unblock");
        let without_unblock = text.replace(
            &block,
            &block.replace("| `block` / `unblock` |", "| `block` |"),
        );
        assert!(readme_gaps(&without_unblock).contains(&"no row for `unblock`".to_string()));
        assert!(readme_gaps(&text).is_empty());
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

    /// One command path in the real clap tree, as the audit records it.
    struct Row {
        path: String,
        kind: &'static str,
        hidden: bool,
        aliases: Vec<String>,
        about: String,
        positionals: Vec<String>,
        flags: Vec<String>,
        /// How many arguments clap declares, so a renderer that drops one
        /// is caught rather than trusted.
        declared: usize,
    }

    fn arg_entry(a: &clap::Arg) -> String {
        let id = a.get_id().as_str();
        let hidden = if a.is_hide_set() { " (hidden)" } else { "" };
        if a.is_positional() {
            let name = a
                .get_value_names()
                .and_then(|v| v.first().map(|s| s.to_string()))
                .unwrap_or_else(|| id.to_uppercase());
            let shape = if a.is_last_set() {
                format!("-- {name}…")
            } else if a.is_required_set() {
                format!("<{name}>")
            } else {
                format!("[{name}]")
            };
            return format!("{shape}{hidden}");
        }
        let mut names: Vec<String> = Vec::new();
        if let Some(s) = a.get_short() {
            names.push(format!("-{s}"));
        }
        if let Some(l) = a.get_long() {
            names.push(format!("--{l}"));
        }
        for al in a.get_visible_aliases().unwrap_or_default() {
            names.push(format!("--{al}"));
        }
        if names.is_empty() {
            names.push(id.to_string());
        }
        let required = if a.is_required_set() {
            " (required)"
        } else {
            ""
        };
        format!("{}{required}{hidden}", names.join("/"))
    }

    /// Every command path, hidden ones included, in tree order.
    fn rows() -> Vec<Row> {
        use clap::CommandFactory;
        fn walk(c: &clap::Command, path: &str, rows: &mut Vec<Row>) {
            for sub in c.get_subcommands() {
                let name = sub.get_name();
                if name == "help" {
                    continue;
                }
                let path = if path.is_empty() {
                    name.to_string()
                } else {
                    format!("{path} {name}")
                };
                let kind = if !sub.has_subcommands() {
                    "action"
                } else if sub.is_subcommand_required_set() {
                    "group"
                } else {
                    "group, bare runs"
                };
                let declared: Vec<&clap::Arg> = sub
                    .get_arguments()
                    .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
                    .collect();
                let mut flags: Vec<String> = declared
                    .iter()
                    .filter(|a| !a.is_positional())
                    .map(|a| arg_entry(a))
                    .collect();
                for g in sub.get_groups().filter(|g| g.is_required_set()) {
                    let members: Vec<String> = g.get_args().map(|id| format!("--{id}")).collect();
                    flags.push(format!("one of {} (required)", members.join("/")));
                }
                rows.push(Row {
                    path: path.clone(),
                    kind,
                    hidden: sub.is_hide_set(),
                    aliases: sub.get_all_aliases().map(str::to_string).collect(),
                    about: sub.get_about().map(|s| s.to_string()).unwrap_or_default(),
                    positionals: declared
                        .iter()
                        .filter(|a| a.is_positional())
                        .map(|a| arg_entry(a))
                        .collect(),
                    flags,
                    declared: declared.len(),
                });
                walk(sub, &path, rows);
            }
        }
        let mut rows = Vec::new();
        walk(&Cli::command(), "", &mut rows);
        rows
    }

    /// `docs/cli-audit.md` embeds this table between its inventory
    /// markers, so the audit cannot describe a command set clank does
    /// not have.
    fn inventory() -> String {
        let cell = |s: &str| s.replace('|', "\\|");
        let mut out = vec![
            "| path | kind | hidden | aliases | about | positionals | flags |".to_string(),
            "|---|---|---|---|---|---|---|".to_string(),
        ];
        for r in rows() {
            out.push(format!(
                "| `{}` | {} | {} | {} | {} | {} | {} |",
                r.path,
                r.kind,
                if r.hidden { "hidden" } else { "" },
                cell(&r.aliases.join(", ")),
                cell(&r.about),
                cell(&r.positionals.join(" ")),
                cell(&r.flags.join(" ")),
            ));
        }
        out.join("\n")
    }

    /// The inventory drops no argument clap declares, short-only ones
    /// included: the required `-m` of `feedback write`, `block create`
    /// and `unblock` has no long form.
    #[test]
    fn the_cli_audit_inventory_records_every_argument() {
        let rows = rows();
        for r in &rows {
            assert_eq!(
                r.positionals.len() + r.flags.iter().filter(|f| !f.starts_with("one of ")).count(),
                r.declared,
                "`{}` lost an argument",
                r.path
            );
        }
        for path in ["feedback write", "block create", "unblock"] {
            let row = rows.iter().find(|r| r.path == path).unwrap();
            assert!(
                row.flags.iter().any(|f| f == "-m (required)"),
                "`{path}`: {:?}",
                row.flags
            );
        }
    }

    fn inventory_paths(table: &str) -> Vec<String> {
        table
            .lines()
            .filter_map(|l| l.strip_prefix("| `"))
            .filter_map(|l| l.split('`').next())
            .map(str::to_string)
            .collect()
    }

    fn audit() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("docs/cli-audit.md");
        std::fs::read_to_string(&path).expect("docs/cli-audit.md at the workspace root")
    }

    /// The audit's embedded inventory is the clap tree as it is now.
    /// `CLANK_BLESS_AUDIT=1` rewrites the block in place.
    #[test]
    fn the_cli_audit_inventory_is_the_real_tree() {
        const START: &str = "<!-- inventory:start -->\n";
        const END: &str = "\n<!-- inventory:end -->";
        let doc = audit();
        let (head, rest) = doc.split_once(START).expect("inventory start marker");
        let (embedded, tail) = rest.split_once(END).expect("inventory end marker");
        let real = inventory();
        if embedded != real && std::env::var_os("CLANK_BLESS_AUDIT").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("docs/cli-audit.md");
            std::fs::write(path, format!("{head}{START}{real}{END}{tail}")).unwrap();
            return;
        }
        assert_eq!(embedded, real, "regenerate with CLANK_BLESS_AUDIT=1");
    }

    /// Every command path has exactly one verdict row, and no verdict
    /// names a path clank does not have.
    #[test]
    fn the_cli_audit_judges_every_command_path() {
        let doc = audit();
        let verdicts = doc
            .split("<!-- verdicts:start -->")
            .nth(1)
            .and_then(|r| r.split("<!-- verdicts:end -->").next())
            .expect("verdict markers");
        let judged = inventory_paths(verdicts);
        let real = inventory_paths(&inventory());
        let missing: Vec<_> = real.iter().filter(|p| !judged.contains(p)).collect();
        let unknown: Vec<_> = judged.iter().filter(|p| !real.contains(p)).collect();
        let mut seen = std::collections::HashSet::new();
        let twice: Vec<_> = judged.iter().filter(|p| !seen.insert(*p)).collect();
        assert!(
            missing.is_empty() && unknown.is_empty() && twice.is_empty(),
            "unjudged: {missing:?}; not commands: {unknown:?}; judged twice: {twice:?}"
        );
    }

    /// The children a group's help names as a list, which clap already
    /// prints beneath it: a run of two or more child names linked by
    /// `,` `/` or `|`, each name bare or a backticked command (`pop`,
    /// `clank stash pop`, `auto on|off`). A child mentioned on its own
    /// in prose ("use `read` when you need the body") is guidance, not
    /// a list, and an `Examples:` block is usage.
    fn hand_listed_children(text: &str, group: &str, children: &[&str]) -> Vec<String> {
        let text = text.split("\nExamples:").next().unwrap_or(text);
        let mut flat = String::new();
        for (i, span) in text.split('`').enumerate() {
            if i % 2 == 0 {
                flat.push_str(span);
                continue;
            }
            let mut words = span.split_whitespace().peekable();
            if words.peek() == Some(&"clank") {
                words.next();
            }
            if words.peek() == Some(&group) {
                words.next();
            }
            flat.push_str(words.next().unwrap_or(""));
        }
        for sep in [',', '/', '|'] {
            flat = flat.replace(sep, &format!(" {sep} "));
        }
        let mut named: Vec<String> = Vec::new();
        let mut run: Vec<&str> = Vec::new();
        let mut linked = false;
        let flush = |run: &mut Vec<&str>, named: &mut Vec<String>| {
            if run.len() >= 2 {
                named.extend(run.iter().map(|s| s.to_string()));
            }
            run.clear();
        };
        for tok in flat.split_whitespace() {
            let word = tok.trim_end_matches(['.', ';', ':']);
            if matches!(tok, "," | "/" | "|") {
                if run.is_empty() || linked {
                    flush(&mut run, &mut named);
                    linked = false;
                } else {
                    linked = true;
                }
            } else if children.contains(&word) {
                if !(run.is_empty() || linked) {
                    flush(&mut run, &mut named);
                }
                run.push(word);
                linked = false;
                if word != tok {
                    flush(&mut run, &mut named);
                }
            } else {
                flush(&mut run, &mut named);
                linked = false;
            }
        }
        flush(&mut run, &mut named);
        named.sort();
        named.dedup();
        named
    }

    #[test]
    fn hand_listed_children_tells_a_list_from_prose() {
        let events = ["list", "ack", "reply", "show", "read"];
        assert_eq!(
            hand_listed_children("The github event inbox: list, ack, show", "events", &events),
            ["ack", "list", "show"],
        );
        let stash = ["push", "pop", "show", "drop", "list"];
        assert!(
            !hand_listed_children("`pop` / `show` / `drop` take the one", "stash", &stash)
                .is_empty()
        );
        assert!(
            !hand_listed_children(
                "`auto on|off|status`, with",
                "auto",
                &["on", "off", "status"]
            )
            .is_empty()
        );
        assert_eq!(
            hand_listed_children("The event inbox: list/ack/show", "events", &events),
            ["ack", "list", "show"],
        );
        assert_eq!(
            hand_listed_children("Run `list`, `ack`, and then rest", "events", &events),
            ["ack", "list"],
        );
        assert!(
            hand_listed_children(
                "Use `read` when you need the body. Use `ack` after handling the event.",
                "events",
                &events,
            )
            .is_empty(),
            "children explained one at a time are guidance",
        );
        assert!(
            hand_listed_children(
                "Read or write review feedback",
                "feedback",
                &["write", "read"]
            )
            .is_empty()
        );
        assert!(
            hand_listed_children(
                "Switch this repo's remote on or off",
                "remote",
                &["status", "on", "off"]
            )
            .is_empty()
        );
        assert!(
            hand_listed_children(
                "Set a plan aside\nExamples:\n  clank stash pop\n  `clank stash list`, `clank stash drop`",
                "stash",
                &stash,
            )
            .is_empty(),
            "an Examples block is usage",
        );
    }

    /// No group's help keeps its own copy of the subcommand list clap
    /// prints; the copies are what drifted (docs/cli-audit.md, F3).
    #[test]
    fn no_group_help_lists_its_own_subcommands() {
        use clap::CommandFactory;
        fn walk(c: &clap::Command, found: &mut Vec<String>) {
            for sub in c.get_subcommands().filter(|s| s.has_subcommands()) {
                let children: Vec<&str> = sub
                    .get_subcommands()
                    .map(|s| s.get_name())
                    .filter(|n| *n != "help")
                    .collect();
                for text in [sub.get_about(), sub.get_long_about()]
                    .into_iter()
                    .flatten()
                {
                    let named = hand_listed_children(&text.to_string(), sub.get_name(), &children);
                    if !named.is_empty() {
                        found.push(format!("{}: {named:?}", sub.get_name()));
                    }
                }
                walk(sub, found);
            }
        }
        let mut found = Vec::new();
        walk(&Cli::command(), &mut found);
        assert!(found.is_empty(), "group help lists its children: {found:?}");
    }

    fn visible_children(c: &clap::Command) -> Vec<&clap::Command> {
        c.get_subcommands()
            .filter(|s| !s.is_hide_set() && s.get_name() != "help")
            .collect()
    }

    /// Every group's help, the top level included, as clap renders it
    /// at `width` columns, paired with the children it lists.
    fn rendered_groups(width: usize) -> Vec<(String, String, Vec<(String, String)>)> {
        fn walk(
            c: &clap::Command,
            path: &str,
            width: usize,
            out: &mut Vec<(String, String, Vec<(String, String)>)>,
        ) {
            for sub in visible_children(c)
                .into_iter()
                .filter(|s| s.has_subcommands())
            {
                let path = format!("{path} {}", sub.get_name());
                let children = visible_children(sub)
                    .iter()
                    .map(|k| (k.get_name().to_string(), about_of(k)))
                    .collect();
                let rendered = sub.clone().term_width(width).render_help().to_string();
                out.push((path.clone(), rendered, children));
                walk(sub, &path, width, out);
            }
        }
        let mut root = cli_at(width);
        root.build();
        let children = visible_children(&root)
            .iter()
            .map(|k| (k.get_name().to_string(), about_of(k)))
            .collect();
        let mut out = vec![(
            "clank".to_string(),
            root.clone().render_help().to_string(),
            children,
        )];
        walk(&root, "clank", width, &mut out);
        out
    }

    fn about_of(c: &clap::Command) -> String {
        c.get_about().map(|a| a.to_string()).unwrap_or_default()
    }

    /// Every visible top-level command sits in exactly one section, and
    /// the sections name nothing else.
    #[test]
    fn the_help_sections_hold_every_visible_command_once() {
        let cmd = Cli::command();
        let visible: Vec<&str> = visible_children(&cmd)
            .iter()
            .map(|c| c.get_name())
            .collect();
        let listed: Vec<&str> = SECTIONS
            .iter()
            .flat_map(|(_, n)| n.iter().copied())
            .collect();
        let missing: Vec<_> = visible.iter().filter(|v| !listed.contains(v)).collect();
        let stray: Vec<_> = listed.iter().filter(|l| !visible.contains(l)).collect();
        let mut seen = std::collections::HashSet::new();
        let twice: Vec<_> = listed.iter().filter(|l| !seen.insert(**l)).collect();
        assert!(
            missing.is_empty() && stray.is_empty() && twice.is_empty(),
            "unsectioned: {missing:?}; not visible commands: {stray:?}; twice: {twice:?}"
        );
    }

    /// At 80 columns every summary fits on its row: the row for each
    /// child carries its whole about, so nothing wraps.
    #[test]
    fn every_summary_fits_one_line_at_80_columns() {
        let mut long = Vec::new();
        for (path, rendered, children) in rendered_groups(80) {
            for (name, about) in children {
                let row = rendered
                    .lines()
                    .find(|l| l.trim_start().split_whitespace().next() == Some(name.as_str()))
                    .unwrap_or_default();
                if !row.contains(about.as_str()) {
                    long.push(format!("{path} {name}: {about}"));
                }
            }
        }
        assert!(
            long.is_empty(),
            "summaries that wrap at 80 columns:\n{}",
            long.join("\n")
        );
    }

    /// A summary is a sentence-case phrase: no trailing period, and it
    /// starts with a capital or a quoted name.
    #[test]
    fn every_summary_is_a_phrase() {
        fn walk(c: &clap::Command, path: &str, bad: &mut Vec<String>) {
            for sub in visible_children(c) {
                let path = format!("{path} {}", sub.get_name());
                let about = about_of(sub);
                let first = about.chars().next();
                if about.is_empty()
                    || about.ends_with('.')
                    || !first.is_some_and(|c| c.is_uppercase() || c == '`')
                {
                    bad.push(format!("{path}: {about:?}"));
                }
                walk(sub, &path, bad);
            }
        }
        let mut bad = Vec::new();
        walk(&Cli::command(), "clank", &mut bad);
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// In a narrow pane the summaries wrap instead of running off the
    /// edge, beside their names or, below the side-by-side minimum, under
    /// them, and none of their words is lost. Only a word wider than the
    /// pane itself may overflow it.
    #[test]
    fn the_top_level_wraps_in_a_narrow_pane() {
        use unicode_width::UnicodeWidthStr;
        for width in [24, 30, 33, 44] {
            let mut root = cli_at(width);
            root.build();
            let rendered = root.clone().render_help().to_string();
            for line in rendered.lines() {
                let indent = line.len() - line.trim_start().len();
                let unbreakable = line.split_whitespace().count() == 1
                    && line.trim().width() > width.saturating_sub(indent);
                assert!(
                    line.width() <= width || unbreakable,
                    "{width} columns, too wide: {line:?}"
                );
            }
            let flat: Vec<&str> = rendered.split_whitespace().collect();
            for child in visible_children(&root) {
                let about = about_of(child);
                let words: Vec<&str> = about.split_whitespace().collect();
                assert!(
                    flat.windows(words.len()).any(|w| w == words.as_slice()),
                    "{width} columns: `{}` lost words: {about}",
                    child.get_name()
                );
            }
        }
    }

    /// Styled where colour is wanted, plain where it is not: the same
    /// help carries escapes only in its ANSI form.
    #[test]
    fn the_help_is_styled_only_when_colour_is_on() {
        let mut root = cli_at(80);
        root.build();
        let help = root.clone().render_help();
        let header = STYLES.get_header().render().to_string();
        assert!(
            help.ansi().to_string().contains(&header),
            "headings and names are styled"
        );
        assert!(
            !help.to_string().contains('\u{1b}'),
            "plain output has no escapes"
        );
        let queue = root.find_subcommand_mut("queue").unwrap().render_help();
        assert!(
            queue.ansi().to_string().contains(&header),
            "subcommands share the styles"
        );
    }

    /// Every `Examples:` line in any command's help parses as that
    /// command. Parsing, not running: the examples name repos and plans
    /// that a test has no business creating.
    #[test]
    fn every_example_parses_as_its_command() {
        fn walk(c: &clap::Command, path: &[String], found: &mut Vec<(Vec<String>, String)>) {
            for sub in c.get_subcommands() {
                let mut path = path.to_vec();
                path.push(sub.get_name().to_string());
                let text = sub
                    .get_after_long_help()
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                if let Some(block) = text.split("Examples:").nth(1) {
                    for line in block.lines().map(str::trim) {
                        if line.starts_with("clank ") {
                            found.push((path.clone(), line.to_string()));
                        }
                    }
                }
                walk(sub, &path, found);
            }
        }
        let mut found = Vec::new();
        walk(&Cli::command(), &[], &mut found);
        assert!(
            found.len() >= 7,
            "the most-typed commands carry examples: {found:?}"
        );
        for (path, line) in found {
            let argv = shell_words::split(&line).unwrap();
            let matches = Cli::command()
                .try_get_matches_from(&argv)
                .unwrap_or_else(|e| panic!("`{line}` does not parse: {e}"));
            let mut at = &matches;
            for name in &path {
                let (sub, m) = at
                    .subcommand()
                    .unwrap_or_else(|| panic!("`{line}` is not `{}`", path.join(" ")));
                assert_eq!(sub, name, "`{line}` is not `{}`", path.join(" "));
                at = m;
            }
        }
    }

    #[test]
    fn queue_unqueue_parses() {
        let parsed = Cli::try_parse_from(["clank", "queue", "unqueue", "foo"]).unwrap();
        let Command::Queue(q) = parsed.command else {
            panic!("expected queue");
        };
        assert!(matches!(
            q.command,
            Some(crate::cli::QueueCmd::Unqueue(crate::cli::QueueUnqueueArgs { ref name })) if name == "foo"
        ));
    }

    #[test]
    fn fork_fresh_parses_and_is_off_by_default() {
        let create = |argv: &[&str]| {
            let parsed = Cli::try_parse_from(argv.iter().copied()).unwrap();
            let Command::Fork(args) = parsed.command else {
                panic!("expected fork");
            };
            let crate::cli::ForkCmd::Create(create) = args.command else {
                panic!("expected create");
            };
            create
        };
        assert!(!create(&["clank", "fork", "create", "spike"]).fresh);
        assert!(create(&["clank", "fork", "create", "spike", "--fresh"]).fresh);
    }

    #[test]
    fn the_readme_lists_the_ci_wake_source() {
        // A reviewer should be able to configure one from the README
        // alone, including the part that surprises people: an arm
        // sees the current result, not a backlog.
        let readme: String = include_str!("../../../../README.md")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(readme.contains("ci_passed"), "the kinds are listed");
        assert!(readme.contains(r#""prs":[7,9]"#), "and the scope");
        assert!(
            readme.contains("is not recoverable"),
            "and what a state read cannot do",
        );
    }

    #[test]
    fn feedback_read_refuses_a_plan_and_a_commit_together() {
        assert!(
            Cli::try_parse_from([
                "clank", "feedback", "read", "--plan", "foo", "--commit", "abc1234",
            ])
            .is_err(),
            "a plan's cycle and one commit are different questions",
        );
    }

    #[test]
    fn bare_feedback_read_plan_is_the_active_plan() {
        let read = |argv: &[&str]| {
            let parsed = Cli::try_parse_from(argv.iter().copied()).unwrap();
            let Command::Feedback(args) = parsed.command else {
                panic!("expected feedback");
            };
            let crate::cli::FeedbackCmd::Read(read) = args.command else {
                panic!("expected read");
            };
            read
        };
        let bare = read(&["clank", "feedback", "read", "--plan"]);
        assert_eq!(bare.plan.as_deref(), Some(""));
        assert!(!bare.json);

        let flagged = read(&["clank", "feedback", "read", "--plan", "-j"]);
        assert_eq!(flagged.plan.as_deref(), Some(""), "-j is not a plan name");
        assert!(flagged.json);

        assert_eq!(
            read(&["clank", "feedback", "read", "--plan", "foo"])
                .plan
                .as_deref(),
            Some("foo"),
        );
        assert_eq!(read(&["clank", "feedback", "read"]).plan, None);
    }

    #[test]
    fn wait_command_parses() {
        assert!(matches!(
            Cli::try_parse_from(["clank", "wait"]).unwrap().command,
            Command::Wait(_)
        ));
        assert!(Cli::try_parse_from(["clank", "grok-wait"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["clank", "wait", "--repo", "."])
                .unwrap()
                .command,
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
            assert_eq!(w.wait.r#for, Some(want));
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
