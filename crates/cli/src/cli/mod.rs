//! Operator-facing CLI commands. Clank is daemonless: each
//! subcommand folds the cwd-repo locally (via the sans-io fold in
//! `clank-core::repo_state`), projects a typed preview (see
//! `crate::preview`), and only then performs mutations via local
//! `git` subprocess calls and direct filesystem writes. The
//! preview is the contract every mutation runs against — it makes
//! "what would happen" inspectable before "do it" runs.

use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

pub mod agent;
pub mod as_cmd;
pub mod attending;
pub mod auto;
pub mod block;
pub mod command;
pub mod config;
pub mod diff;
pub mod doctor;
pub mod events;
pub mod export;
pub mod feedback;
pub mod finish;
pub mod fork;
pub(crate) mod github_event_log;
pub mod github_events;
pub(crate) mod github_timeline;
pub mod html;
pub mod html_highlight;
pub mod init;
pub mod log;
pub mod open;
pub mod open_zellij;
pub mod pick;
pub mod plan_resolve;
pub mod pr_review;
pub mod purge;
pub mod queue;
pub mod rereview;
pub mod rewire;
pub mod rewrite;
pub mod run;
pub(crate) mod session_holder;
pub mod setup;
pub mod stash;
pub mod status;
pub(crate) mod status_tui;
pub mod stop_hook;
pub mod team;
pub mod teams_config;
pub(crate) mod term;
pub mod unfinish;
pub mod wait;

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigKey>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    #[arg(short = 'j', long)]
    pub json: bool,
    /// Write to the USER-scope config (`~/.clank/config.json`) instead of the
    /// repo's `.clank/config.json`. Only affects `set`; reads always show the
    /// effective merged value (repo shadows user).
    #[arg(long)]
    pub global: bool,
}

#[derive(Args, Debug)]
pub struct ConfigKeyArgs {
    /// Action: get or set
    pub action: Option<String>,
    /// Value (for set)
    pub value: Option<String>,
}

#[derive(clap::Subcommand, Debug)]
pub enum ConfigKey {
    /// Require review for ad-hoc (non-plan) commits. bool, default: false.
    #[command(name = "review.adhoc_feedback", alias = "review_adhoc_feedback")]
    ReviewAdhocFeedback(ConfigKeyArgs),
    /// Require review for plan-attributed commits. bool, default: true.
    #[command(name = "review.plan_feedback", alias = "review_plan_feedback")]
    ReviewPlanFeedback(ConfigKeyArgs),
    /// Require [plan] or [misc] commit title prefixes. bool, default: false.
    #[command(
        name = "review.require_commit_prefix",
        alias = "review_require_commit_prefix"
    )]
    ReviewRequireCommitPrefix(ConfigKeyArgs),
    /// Shell command to run when master has new work. string or null.
    #[command(name = "hooks.master_work", alias = "hooks_master_work")]
    HooksMasterWork(ConfigKeyArgs),
    /// Shell command to run when a reviewer has work. string or null.
    #[command(name = "hooks.reviewer_work", alias = "hooks_reviewer_work")]
    HooksReviewerWork(ConfigKeyArgs),
    /// Shell command to run when a plan is finished. string or null.
    #[command(name = "hooks.plan_finalized", alias = "hooks_plan_finalized")]
    HooksPlanFinalized(ConfigKeyArgs),
    /// Shell command to run on idle (no work). string or null.
    #[command(name = "hooks.idle", alias = "hooks_idle")]
    HooksIdle(ConfigKeyArgs),
    /// Shell command to run when an agent creates a block. string or null.
    #[command(name = "hooks.blocked", alias = "hooks_blocked")]
    HooksBlocked(ConfigKeyArgs),
    /// Editor executable for `clank diff`. string or null.
    #[command(name = "diff.editor.command", alias = "diff_editor_command")]
    DiffEditorCommand(ConfigKeyArgs),
    /// Default `--wait` behavior for `clank diff`. bool, default: false.
    #[command(name = "diff.wait", alias = "diff_wait")]
    DiffWait(ConfigKeyArgs),
    /// Auto-squash a plan into ONE commit at `clank finish`, REWRITING the
    /// current branch in place (rewrites published history if already pushed).
    /// bool, default: false. `--no-squash` skips it for one finish.
    #[command(name = "finish.autosquash", alias = "finish_autosquash")]
    FinishAutosquash(ConfigKeyArgs),
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Overwrite an existing foreign `post-rewrite` hook.
    #[arg(long)]
    pub force_hooks: bool,
    /// With `--team <name>`: replace an existing VALID repo roster.
    /// Without it, `--team` refuses to clobber a configured repo
    /// roster (run `clank team save <name>` first to keep local
    /// edits). Bare `clank init` ignores this flag.
    #[arg(long)]
    pub force: bool,
    /// Seed this repo's roster from a user-scope team template.
    /// Copies the named template's roster + referenced agent
    /// definitions into `<repo>/.clank/config.json`. The template
    /// must exist in `~/.clank/config.json#/teams`. When omitted, the
    /// repo starts with no roster — build one with `clank agent add`
    /// / `clank agent promote`; workflow commands (`wait`,
    /// `finish`) require a master until you do.
    #[arg(long, value_name = "NAME")]
    pub team: Option<String>,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ForkArgs {
    #[command(subcommand)]
    pub command: ForkCmd,
}

/// `fork` is a NOUN with a lifecycle, not a verb. Every other durable
/// multi-operation noun in this CLI uses subcommands, and the bare
/// positional form made `clank fork list` create a worktree called
/// `list` — a silently successful wrong action (fork-cli-is-a-noun).
#[derive(Subcommand, Debug)]
pub enum ForkCmd {
    /// Fork the team into a linked worktree (or `--clone`).
    Create(ForkCreateArgs),
    /// List this repo's forks.
    List(ForkListArgs),
    /// Remove a fork's worktree or clone.
    Remove(ForkRemoveArgs),
}

#[derive(Args, Debug)]
pub struct ForkListArgs {
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ForkRemoveArgs {
    /// Fork name, resolved through the validated fork identity — never
    /// a path.
    pub name: String,
    /// Override the dirty-state and unreachable-commit refusals. Never
    /// overrides the identity check. For a CLONE this permanently
    /// destroys commits the source cannot reach.
    #[arg(long)]
    pub force: bool,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ForkCreateArgs {
    /// Worktree name — also the new branch name and the zellij
    /// tab name. Defaults to `pr-<N>` when `--pr` is given.
    #[arg(required_unless_present = "pr")]
    pub name: Option<String>,
    /// Repo to fork from. Defaults to the cwd's git toplevel.
    pub source: Option<PathBuf>,
    /// Fork onto a GitHub PR: fetches `pull/<N>/head`, bases the
    /// worktree on the pinned head sha, and orients the forked
    /// sessions to "reviewing PR #<N>: <title>".
    #[arg(long, value_name = "N", conflicts_with = "branch")]
    pub pr: Option<u32>,
    /// Base the worktree's new branch on this ref instead of
    /// source HEAD (e.g. a fetched PR head).
    #[arg(long, value_name = "REF")]
    pub branch: Option<String>,
    /// Worktree destination. Defaults to
    /// `<source>/.clank/worktrees/<name>`.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,
    /// Full local CLONE at `.clank/clones/<name>` instead of a
    /// linked worktree: an independent repo whose only remote is
    /// the local main root (removable — that's the point).
    /// Conflicts with `--path` (clones are never relocated; the
    /// descriptor, not the path, is the identity) and `--pr` (a
    /// fetched PR head is reachable from no main-root ref, and git
    /// does not guarantee unreachable objects survive a clone).
    #[arg(long, conflicts_with_all = ["path", "pr"])]
    pub clone: bool,
    /// Seed the fork's roster from this user-scope team template
    /// (`~/.clank/config.json#/teams`) instead of copying the source
    /// repo's roster. Members with a bound session in the source get
    /// their sessions forked; the rest start fresh.
    #[arg(long, value_name = "NAME")]
    pub team: Option<String>,
    /// Move a draft from the source repo's `.clank/drafts/` into the
    /// fork's queue. Repeatable — the first named draft gets the lowest
    /// priority (000, then 001, …), so the forked master is handed them
    /// in the order given. `.md` suffix optional.
    #[arg(short = 'd', long = "draft", value_name = "NAME")]
    pub drafts: Vec<String>,
    /// Extra orienting context for the forked sessions (appended
    /// to the standard orientation prompt).
    #[arg(long, value_name = "TEXT")]
    pub prompt: Option<String>,
    /// Skip the default tab-opening when run inside zellij.
    /// Outside zellij fork never auto-spawns a session.
    #[arg(long)]
    pub no_open: bool,
    /// Also scaffold the PR review in the new worktree (requires
    /// `--pr`). Same as `clank pr-review start <N> --fork`.
    #[arg(long, requires = "pr")]
    pub review: bool,
}

#[derive(Args, Debug)]
pub struct RereviewArgs {
    /// Plan to re-open. Defaults to the active plan.
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct RewireArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Read git `post-rewrite` pairs (`<old> <new> [extra]`,
    /// one per line) from stdin and rewire feedback in place.
    #[arg(long)]
    pub from_stdin: bool,
}

#[derive(Args, Debug)]
pub struct LogArgs {
    /// Commit range (git-log style). `<sha>` = from sha to HEAD.
    /// `<from>..<to>` = exclusive from, inclusive to. Omit for
    /// the last `-n` commits from HEAD.
    pub range: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    #[arg(short = 'j', long)]
    pub json: bool,
    #[arg(long, value_name = "PLAN")]
    pub plan: Option<String>,
    /// Number of commits to fold from HEAD (default 30).
    #[arg(short = 'n', default_value_t = 30)]
    pub limit: usize,
    /// Compact one-line-per-commit output.
    #[arg(long)]
    pub oneline: bool,
    /// Leave github events out of the timeline
    /// (log-timeline-github-events).
    #[arg(long)]
    pub no_github: bool,
}

#[derive(Args, Debug)]
pub struct HtmlArgs {
    #[command(subcommand)]
    pub command: Option<HtmlCmd>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Ignore the cached `clank:last-built-sha` marker and
    /// re-render every page from scratch.
    #[arg(long)]
    pub rebuild: bool,
    /// Suppress the progress bar (also auto-suppressed when
    /// stderr isn't a TTY).
    #[arg(long)]
    pub quiet: bool,
}

#[derive(clap::Subcommand, Debug)]
pub enum HtmlCmd {
    /// Build the site, then launch the host browser
    /// (`open` / `xdg-open` / `explorer`) on the generated
    /// `index.html` — or on `plan/<stem>.html` if a plan name
    /// is supplied.
    Open(HtmlOpenArgs),
}

#[derive(clap::Args, Debug)]
pub struct HtmlOpenArgs {
    /// Plan name. When supplied, the browser opens
    /// `plan/<stem>.html` instead of `index.html`. Resolved
    /// against active plans first, then finished — same shape
    /// as `clank diff <plan>`.
    pub plan: Option<String>,
    /// Commit sha (short or full). When supplied, the browser
    /// opens `commit/<full_sha>.html`. Mutually exclusive with a
    /// plan name.
    #[arg(long, conflicts_with = "plan", value_name = "SHA")]
    pub commit: Option<String>,
    /// Queued plan name. When supplied, the browser opens
    /// `queue/<name>.html`. Mutually exclusive with a plan name
    /// or commit.
    #[arg(
        long,
        conflicts_with = "plan",
        conflicts_with = "commit",
        value_name = "NAME"
    )]
    pub queue: Option<String>,
    /// Stashed plan name. When supplied, the browser opens
    /// `stash/<name>.html`. Mutually exclusive with the other targets.
    #[arg(
        long,
        conflicts_with = "plan",
        conflicts_with = "commit",
        conflicts_with = "queue",
        value_name = "NAME"
    )]
    pub stash: Option<String>,
    /// Print the resolved target path on stdout and exit
    /// without launching a browser. Mirrors `--print` on
    /// `clank agent start`, `clank diff`, `clank open zellij`.
    #[arg(long)]
    pub print_path: bool,
}

/// `clank open` — bare form opens the zellij agent workspace
/// (context-aware: new tab in-session, attach-or-create outside;
/// `clank-open-zellij-context`). Subcommands: `dry` = the
/// read-only path classifier; `zellij` = the opener, explicit.
#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct OpenArgs {
    #[command(subcommand)]
    pub command: Option<OpenCmd>,
    /// Bare `clank open` IS the zellij opener: a tab inside a live
    /// session, attach-or-create outside one. The SAME OpenZellijArgs
    /// is flattened here so the bare form's flags can't drift from the
    /// `zellij` subcommand's (ruthless 8c2206d).
    #[command(flatten)]
    pub zellij: OpenZellijArgs,
}

#[derive(clap::Subcommand, Debug)]
pub enum OpenCmd {
    /// Classify a path against the active repo; emit JSON or
    /// human-readable. Renamed from `clank open <path>`.
    Dry(OpenDryArgs),
    /// Auto-generate a zellij layout (KDL) for master + reviewer
    /// panes and spawn it via `zellij --layout`.
    ///
    /// Inside an existing zellij session, the layout opens as a
    /// new tab (no new session is spawned). Outside any session,
    /// it starts a new session. This is zellij's `--layout`
    /// behavior — no extra detection in clank.
    ///
    /// Use `--print` to emit the composed KDL on stdout + the
    /// would-be-spawned argv on stderr instead of shelling out.
    Zellij(OpenZellijArgs),
}

#[derive(Args, Debug)]
pub struct OpenDryArgs {
    /// Path to inspect. Echoed back verbatim as `requested_path`;
    /// not canonicalized until existence is confirmed. The repo
    /// root is derived from this path's `git rev-parse --show-toplevel`,
    /// so no `--repo` flag is needed — codex caught on 30a0194
    /// that adding one would have been a silent no-op.
    pub path: String,
    /// Emit the response as JSON.
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct OpenZellijArgs {
    /// Repo root override. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Open an EXISTING fork (linked worktree under
    /// `.clank/worktrees/<name>`) as a tab, idempotently — no-op if
    /// its tab is already open. Errors if the fork doesn't exist
    /// (`clank fork create <name>` creates it).
    #[arg(long, value_name = "NAME", conflicts_with_all = ["pr", "all"])]
    pub fork: Option<String>,
    /// Open the existing `pr-<N>` fork's tab (same as `--fork pr-<N>`).
    #[arg(long, value_name = "N", conflicts_with_all = ["fork", "all"])]
    pub pr: Option<u32>,
    /// Open a tab for every fork under `.clank/worktrees/` that isn't
    /// already open. Heavy: each fork spawns its full team — use
    /// deliberately.
    #[arg(long, conflicts_with_all = ["fork", "pr"])]
    pub all: bool,
    /// Emit the composed KDL on stdout + the would-be-spawned
    /// argv on stderr, exit 0, don't shell out to zellij. Mirrors
    /// `clank diff --print` / `clank agent start --print`.
    #[arg(long)]
    pub print: bool,
}

/// `clank diff <plan|range>` — launch the configured editor on
/// a plan or commit range diff.
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Plan name OR git commit range. Auto-resolves: try plan-resolve
    /// first, then range. Use `--plan` / `--range` for an explicit
    /// override when the heuristic guesses wrong.
    pub target: Option<String>,
    /// Explicit plan name (mutually exclusive with `--range` and
    /// the positional target).
    #[arg(long, conflicts_with_all = ["range", "target"])]
    pub plan: Option<String>,
    /// Explicit git commit range (mutually exclusive with `--plan`
    /// and the positional target).
    #[arg(long, conflicts_with_all = ["plan", "target"])]
    pub range: Option<String>,
    /// Agent-supplied hint about what's interesting in this diff.
    /// Surfaced to the editor via `CLANK_DIFF_PROMPT` and the
    /// `{prompt}` template variable.
    #[arg(long, value_name = "TEXT")]
    pub prompt: Option<String>,
    /// File region of interest. Format: `path/to/file:start-end`,
    /// `path/to/file:line` (shorthand for `line-line`), or
    /// `path/to/file` (whole file). Repeatable. Surfaced via
    /// `CLANK_DIFF_FOCUS` (newline-separated).
    #[arg(long, value_name = "FILE[:LINES]")]
    pub focus: Vec<String>,
    /// Wait for the editor to exit before returning. Overrides
    /// `diff.wait` from config.
    #[arg(long, conflicts_with = "no_wait")]
    pub wait: bool,
    /// Fire-and-forget. Overrides `diff.wait: true` from config.
    #[arg(long = "no-wait", conflicts_with = "wait")]
    pub no_wait: bool,
    /// Print the composed launch line (program + shell-quoted args
    /// on stdout, env additions on stderr) and exit 0 without
    /// spawning. Mirrors `clank agent start --print`.
    #[arg(long)]
    pub print: bool,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Emit JSON instead of the human rendering.
    #[arg(short = 'j', long)]
    pub json: bool,
    /// Skip the on-disk state cache: don't read it, don't write it.
    /// Useful for real-world A/B timing against a warm cache and as
    /// a debug escape hatch.
    #[arg(long)]
    pub no_cache: bool,
    /// Specific plan to render. Accepts `<stem>`, `<stem>.md`, or
    /// `<basename>/<stem>.md` — same parser as `clank finish`.
    #[arg(long, value_name = "PLAN")]
    pub plan: Option<String>,
    /// Watch for changes and re-print status when it changes.
    /// Never exits on its own. With `-j`, emits one compact JSON
    /// line per update.
    #[arg(long)]
    pub watch: bool,
    /// Full-screen read-only live status view, sized for a small
    /// zellij pane. No input handling — close the pane (or Ctrl-C)
    /// to exit. Conflicts with --json/--watch/--plan.
    #[arg(long, conflicts_with_all = ["json", "watch", "plan"])]
    pub tui: bool,
}

#[derive(Args, Debug)]
pub struct WaitArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Agent label this caller is waiting as. Keys feedback files
    /// and the participant set. Optional: defaults via the shared
    /// identity resolver (CLANK_AGENT env > session lookup via
    /// CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID). Pass explicitly
    /// to override or to run from outside a session.
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
    /// Exit when the process that spawned this wait goes away.
    ///
    /// The spawner must hand this process a pipe as stdin and hold
    /// the write end; EOF then means the owner is gone, which the
    /// kernel reports on an orderly exit and a SIGKILL alike. Opt-in
    /// on purpose: without it stdin is ignored, so an interactive
    /// `clank wait < /dev/null` does not exit the moment it starts.
    #[arg(long)]
    pub die_with_owner: bool,
    /// Emit JSON instead of the human rendering.
    #[arg(short = 'j', long)]
    pub json: bool,
    /// Extra wake source for THIS wait, as the agent-config item JSON
    /// (`{"kind":"github",…}` / `{"kind":"command",…}`) — exactly the
    /// `wait_events` config shape, merged after the config's entries.
    /// Repeatable.
    #[arg(long = "event", value_name = "JSON")]
    pub events: Vec<String>,
    /// OBSERVE the repo instead of waiting for own work: block until
    /// the given event lands, counting only events AFTER the wait
    /// starts. For watching a foreign repo (pair with --repo) — no
    /// session binding is needed there, so --author is ignored.
    /// Fires no lifecycle hooks.
    #[arg(
        long = "for",
        value_enum,
        value_name = "EVENT",
        conflicts_with = "peek"
    )]
    pub r#for: Option<WaitFor>,
    /// One-shot, non-blocking probe: report whether the caller has
    /// actionable work RIGHT NOW and return immediately — never enter the
    /// watcher loop. Unlike a real wait it is SIDE-EFFECT-FREE: it fires no
    /// lifecycle or idle hooks and promotes nothing. Emits the same output
    /// (empty when there's no work). Used by the Stop hook to tell "still
    /// your turn" from "idle" without blocking.
    #[arg(long)]
    pub peek: bool,
    /// Skip the on-disk state cache.
    #[arg(long)]
    pub no_cache: bool,
    /// Force polling mode for git changes (skip the native gitdir
    /// watch, refold on a 500ms tick). Use this when native
    /// `notify` events on the gitdir are unreliable — most
    /// notably inside the Codex tool sandbox.
    ///
    /// Default is `false`. Unset, the default flips to `true`
    /// when `CODEX_SANDBOX=seatbelt` is in the environment. Pass
    /// `--no-poll` to force the native path regardless of env.
    #[arg(long, overrides_with = "no_poll")]
    pub poll: bool,
    #[arg(long, overrides_with = "poll")]
    pub no_poll: bool,
}

impl WaitArgs {
    /// Resolve the explicit poll flag (Some) vs absent (None) for
    /// `resolve_poll`. clap can't directly produce
    /// `Option<bool>` with `--poll` / `--no-poll`, so we
    /// reconstruct here.
    pub fn explicit_poll(&self) -> Option<bool> {
        match (self.poll, self.no_poll) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }

    /// Effective poll setting. Reads `CODEX_SANDBOX` if the
    /// caller didn't pass `--poll` / `--no-poll`. THE single
    /// place in the binary that reads `CODEX_SANDBOX` —
    /// everything downstream takes a plain `bool`.
    pub fn effective_poll(&self) -> bool {
        let codex = std::env::var("CODEX_SANDBOX").ok();
        resolve_poll(self.explicit_poll(), codex.as_deref())
    }
}

/// Pure decision function — no env access, no I/O. `explicit`
/// is the `--poll` / `--no-poll` selection if present; the env
/// fallback applies only when it's `None`.
pub fn resolve_poll(explicit: Option<bool>, codex_sandbox: Option<&str>) -> bool {
    explicit.unwrap_or_else(|| codex_sandbox == Some("seatbelt"))
}

#[cfg(test)]
mod role_arg_alias_tests {
    use super::RoleArg;
    use clap::ValueEnum;

    #[test]
    fn role_arg_accepts_canonical_singular() {
        let parsed = RoleArg::from_str("reviewer", false).unwrap();
        assert!(matches!(parsed, RoleArg::Reviewer));
    }

    #[test]
    fn role_arg_accepts_plural_alias() {
        // Plan acceptance: `--role reviewers` keeps working via
        // the clap alias.
        let parsed = RoleArg::from_str("reviewers", false).unwrap();
        assert!(matches!(parsed, RoleArg::Reviewer));
    }

    #[test]
    fn role_arg_rejects_unrelated_strings() {
        assert!(RoleArg::from_str("xyzzy", false).is_err());
    }
}

#[cfg(test)]
mod resolve_poll_tests {
    use super::resolve_poll;

    #[test]
    fn explicit_true_wins_regardless_of_env() {
        assert!(resolve_poll(Some(true), None));
        assert!(resolve_poll(Some(true), Some("seatbelt")));
        assert!(resolve_poll(Some(true), Some("anything-else")));
    }

    #[test]
    fn explicit_false_wins_regardless_of_env() {
        assert!(!resolve_poll(Some(false), None));
        assert!(!resolve_poll(Some(false), Some("seatbelt")));
    }

    #[test]
    fn unset_with_codex_sandbox_seatbelt_defaults_to_true() {
        assert!(resolve_poll(None, Some("seatbelt")));
    }

    #[test]
    fn unset_with_empty_codex_sandbox_defaults_to_false() {
        assert!(!resolve_poll(None, Some("")));
    }

    #[test]
    fn unset_with_other_codex_sandbox_value_defaults_to_false() {
        assert!(!resolve_poll(None, Some("other-value")));
    }

    #[test]
    fn unset_with_no_codex_sandbox_defaults_to_false() {
        assert!(!resolve_poll(None, None));
    }
}

/// Observer events for `clank wait --for`
/// (wait-for-observer-mode). Delta-from-startup: each fires only
/// for occurrences after the wait began.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum WaitFor {
    /// HEAD moved (any new commit).
    Commit,
    /// A plan was finalized.
    Finished,
    /// The repo came to a stop-point: a plan finalized OR a new
    /// unanswered block appeared (someone needs the human).
    Stopped,
}

#[derive(Args, Debug)]
pub struct EventsArgs {
    #[command(subcommand)]
    pub command: EventsCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum EventsCmd {
    /// List logged github events (unhandled by default).
    List(EventsListArgs),
    /// Mark events handled so they stop waking you.
    Ack(EventsAckArgs),
    /// Show one event's full record.
    Show(EventsShowArgs),
}

#[derive(Args, Debug)]
pub struct EventsListArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Agent whose inbox to read (default: the session's binding).
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
    /// Include handled (acked) events too.
    #[arg(long)]
    pub all: bool,
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct EventsAckArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Agent whose inbox to ack in (default: the session's binding).
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
    /// Event ids from `clank events list` — a bare seq, or
    /// `<source>@<seq>` when several sources share a seq.
    #[arg(required = true, value_name = "ID")]
    pub ids: Vec<String>,
}

#[derive(Args, Debug)]
pub struct EventsShowArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Agent whose inbox to read (default: the session's binding).
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
    /// An id from `clank events list`.
    #[arg(value_name = "ID")]
    pub id: String,
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct FeedbackArgs {
    #[command(subcommand)]
    pub command: FeedbackCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum FeedbackCmd {
    /// Write a feedback file.
    Write(FeedbackWriteArgs),
    /// Show all feedback for a commit.
    Read(FeedbackReadArgs),
}

#[derive(Args, Debug)]
pub struct FeedbackReadArgs {
    /// Commit SHA (default: HEAD).
    #[arg(long, value_name = "SHA")]
    pub commit: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct FeedbackWriteArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Commit ref (7+ lowercase hex chars). Resolved against
    /// known commits; ambiguous prefixes are an error.
    #[arg(long, value_name = "SHA")]
    pub commit: String,
    /// Verdict. Prepended to the body as `CONTINUE <body>` or
    /// `REQUEST_CHANGES <body>`.
    #[arg(long, value_enum)]
    pub verdict: VerdictArg,
    /// Agent label to attribute the feedback to.
    #[arg(long, value_name = "LABEL")]
    pub author: String,
    /// Review message (like `git commit -m`). First line is the
    /// summary; subsequent lines are details. Required. Don't restate
    /// the verdict — a leading `CONTINUE:` / `REQUEST_CHANGES:` etc.
    /// matching --verdict is stripped.
    #[arg(short = 'm', value_name = "MSG")]
    pub message: String,
}

/// Verdict the writer is claiming for this feedback. Maps to
/// [`clank_core::Verdict`] for validation. `Unmarked` is
/// intentionally not selectable — every written file carries a
/// real verdict.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum VerdictArg {
    Continue,
    Finished,
    RequestChanges,
}

impl From<VerdictArg> for clank_core::Verdict {
    fn from(v: VerdictArg) -> Self {
        match v {
            VerdictArg::Continue => clank_core::Verdict::Continue,
            VerdictArg::Finished => clank_core::Verdict::Finished,
            VerdictArg::RequestChanges => clank_core::Verdict::RequestChanges,
        }
    }
}

#[derive(Args, Debug)]
pub struct PrReviewArgs {
    #[command(subcommand)]
    pub command: PrReviewCmd,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(clap::Subcommand, Debug)]
pub enum PrReviewCmd {
    /// Start reviewing a GitHub PR: pin its head sha and scaffold
    /// `.clank/pr-reviews/<pr>/`.
    Start(PrReviewStartArgs),
    /// Record your reviewer verdict for the current round.
    Note(PrReviewNoteArgs),
    /// Open/re-open the review for the current draft: bump the round
    /// and summon reviewers (master-only).
    Propose(PrReviewProposeArgs),
    /// Discard a PR review's local scratch (master-only).
    Abort(PrReviewAbortArgs),
    /// Publish the converged review to the PR (master-only).
    Submit(PrReviewSubmitArgs),
    /// Round + per-reviewer verdicts + who we're waiting on.
    Status(PrReviewStatusArgs),
}

#[derive(Args, Debug)]
pub struct PrReviewSubmitArgs {
    /// The review outcome to publish (GitHub's canonical events).
    /// Required — master must state the verdict.
    #[arg(long, value_enum)]
    pub event: crate::cli::pr_review::ReviewEvent,
    /// PR number. Defaults to the single active review.
    #[arg(long)]
    pub pr: Option<u32>,
}

#[derive(Args, Debug)]
pub struct PrReviewStartArgs {
    /// PR number.
    pub pr: u32,
    /// Create a linked worktree on the PR head + seed the team, then
    /// scaffold the review there (same as `clank fork create --pr <N>
    /// --review`). Mutually exclusive with `--checkout`.
    #[arg(long, conflicts_with = "checkout")]
    pub fork: bool,
    /// Check out the PR head in the CURRENT worktree (branch
    /// `pr-<N>`), then scaffold the review here. Refuses on a dirty
    /// worktree. Mutually exclusive with `--fork`.
    #[arg(long)]
    pub checkout: bool,
}

#[derive(Args, Debug)]
pub struct PrReviewProposeArgs {
    /// PR number. Defaults to the single active review.
    #[arg(long)]
    pub pr: Option<u32>,
}

#[derive(Args, Debug)]
pub struct PrReviewNoteArgs {
    /// Your verdict for the current round.
    #[arg(long, value_enum)]
    pub verdict: VerdictArg,
    /// Review summary (like `git commit -m`).
    #[arg(short = 'm', value_name = "MSG")]
    pub message: String,
    /// PR number. Defaults to the single active review.
    #[arg(long)]
    pub pr: Option<u32>,
    /// Agent label. Defaults to the session-bound identity.
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
}

#[derive(Args, Debug)]
pub struct PrReviewAbortArgs {
    /// PR number. Defaults to the single active review.
    #[arg(long)]
    pub pr: Option<u32>,
}

#[derive(Args, Debug)]
pub struct PrReviewStatusArgs {
    /// PR number. Defaults to the single active review.
    #[arg(long)]
    pub pr: Option<u32>,
}

#[derive(Args, Debug)]
pub struct AsArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// The agent label to bind this session to. Subsequent calls
    /// to `clank wait` / `clank auto` / the stop-hook resolve to
    /// this label for the duration of this agent session.
    pub label: String,
}

#[derive(Args, Debug)]
pub struct AutoArgs {
    #[command(subcommand)]
    pub command: AutoCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum AutoCmd {
    /// Enable auto-mode (wait-for-work long-poll). Optionally
    /// update role.
    On(AutoOnArgs),
    /// Disable auto-mode. Optionally update role.
    Off(AutoOffArgs),
    /// Print current auto-mode + role for the calling agent.
    Status(AutoStatusArgs),
}

#[derive(Args, Debug)]
pub struct AutoOnArgs {
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Designate this agent as `master` or `reviewer` for the
    /// repo. `master` writes `.clank/config.json` with this
    /// label; `reviewer` clears master only if this agent
    /// currently holds it.
    #[arg(long, value_enum)]
    pub role: Option<RoleArg>,
}

#[derive(Args, Debug)]
pub struct AutoOffArgs {
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub role: Option<RoleArg>,
}

#[derive(Args, Debug)]
pub struct AutoStatusArgs {
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Emit JSON instead of the human rendering.
    #[arg(short = 'j', long)]
    pub json: bool,
}

/// Role designation for `--role`.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum RoleArg {
    Master,
    /// Singular per `role-reviewers-to-reviewer-rename`.
    /// `alias = "reviewers"` preserves backwards compat for
    /// users typing `--role reviewers`.
    #[clap(alias = "reviewers")]
    Reviewer,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Repo root override. Defaults to the cwd's git toplevel
    /// (which falls back to "no repo" if cwd isn't in a git
    /// checkout — repo checks are skipped in that case).
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Emit JSON instead of the human rendering.
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Overwrite skill/command files even if they've drifted
    /// from the embedded content (e.g. user manually edited).
    /// Hook entries in settings.json / hooks.json are always
    /// tag-merged regardless of this flag.
    #[arg(long)]
    pub force: bool,
    /// Show what would change without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct StopHookArgs {
    /// Which agent CLI is invoking this hook. Set by
    /// `clank setup` in the hook config; agents don't pass it
    /// themselves. Used to render the per-tool continuation
    /// wire shape (claude: exit 2 + stderr; codex: exit 0 +
    /// stdout JSON).
    #[arg(long, value_enum)]
    pub tool: ToolArg,
    /// Repo root override. Defaults to the cwd reported in the
    /// hook stdin JSON (which IS the agent's cwd at stop time).
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// SessionStart handler (claude-asyncrewake-work-loop): mint a
    /// new wait generation (revoking stale waiters) and surface any
    /// pending work as additionalContext. Installed alongside the
    /// asyncrewake Stop entry; never fails a session start.
    #[arg(long)]
    pub session_start: bool,
    /// Delivery-loop mode, written into the installed entry by
    /// `clank setup` (claude-asyncrewake-work-loop). The hook obeys
    /// ITS OWN argv — never a runtime capability probe — so the
    /// static hook entry and the behavior cannot drift.
    #[arg(long = "loop", value_enum)]
    pub loop_mode: Option<LoopModeArg>,
}

/// `--loop` value for the claude stop hook.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LoopModeArg {
    /// Park the shared long-poll inside an asyncRewake hook; work
    /// wakes the session via exit 2 + stderr.
    Asyncrewake,
}

/// `--tool` value. Mirrors [`clank_core::Tool`].
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum ToolArg {
    Claude,
    Codex,
    Grok,
    /// One word, like the binary (clap would kebab-case the variant).
    #[value(name = "opencode")]
    OpenCode,
}

impl From<ToolArg> for clank_core::Tool {
    fn from(t: ToolArg) -> Self {
        match t {
            ToolArg::Claude => clank_core::Tool::Claude,
            ToolArg::Codex => clank_core::Tool::Codex,
            ToolArg::Grok => clank_core::Tool::Grok,
            ToolArg::OpenCode => clank_core::Tool::OpenCode,
        }
    }
}

#[derive(Args, Debug)]
pub struct QueueArgs {
    #[command(subcommand)]
    pub command: Option<QueueCmd>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

/// Read-only inspection of the repo's registered agents.
#[derive(Args, Debug)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub command: AgentCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum AgentCmd {
    /// Enumerate the repo's roster (master + reviewers) with their
    /// role, review tier, and bind state.
    List(AgentListArgs),
    /// Launch an agent's CLI tool with any configured `launch`
    /// profile applied. With a bound session it RESUMES that
    /// session; without one it bootstraps, exec'ing the bare tool
    /// with a seed prompt telling the agent to run `clank as
    /// <name>` (agent-start-bootstraps-missing-skeleton).
    Start(AgentStartArgs),
    /// Add an agent to the repo's roster (ONE step — definition +
    /// role). Three modes: `--global --tool` writes a reusable
    /// DESCRIPTION to the user-scope library (no repo change);
    /// repo-scope `--tool` defines a fresh agent inline; repo-scope
    /// by-name (no `--tool`) copies a description from the
    /// user-scope library. `--review commit|plan|final|gate` sets
    /// the tier (default `commit`).
    Add(AgentAddArgs),
    /// Promote an agent to the repo's master:
    /// `repo.agents[<name>].role = master`, demoting the previous
    /// master to `commit`. The agent must already be on the roster.
    /// (Distinct from `clank queue promote`, which activates a queued
    /// plan.)
    Promote(AgentPromoteArgs),
    /// Remove an agent from the repo's roster. `--global` instead
    /// drops a DESCRIPTION from the user-scope `agents` library
    /// (and scrubs any team template referencing it). Per-agent
    /// skeleton dir + feedback history are preserved.
    Remove(AgentRemoveArgs),
    /// Change a reviewer's tier (commit / plan / final / gate) in
    /// place — no remove/re-add, so the agent's session and zellij
    /// pane are undisturbed. Refuses on the master (not a reviewer
    /// tier).
    SetReview(AgentSetReviewArgs),
    /// Replace a reviewer with another, keeping the role: one config
    /// write, so the role is never observed vacant and the gate's
    /// expected reviewer set never briefly shrinks. Refuses on the
    /// master (use `agent promote`) and refuses when the incoming
    /// label is already on the roster.
    Swap(AgentSwapArgs),
}

#[derive(Args, Debug)]
pub struct AgentSwapArgs {
    /// Reviewer to swap OUT. Must be on the roster.
    pub out: String,
    /// Reviewer to swap IN. Must NOT already be on the roster; its
    /// description is copied from the user-scope library.
    #[arg(value_name = "IN")]
    pub into: String,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct AgentListArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Emit machine-readable JSON instead of human text.
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct AgentStartArgs {
    /// Agent label to start.
    pub name: String,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Instead of execing the composed command, print the argv
    /// (shell-quoted, on stdout) and the env diff (on stderr)
    /// and exit 0. Useful for `clank open zellij` to introspect
    /// the spawn line, and for integration tests that can't
    /// observe an exec'd process.
    #[arg(long)]
    pub print: bool,
}

#[derive(Args, Debug)]
pub struct AgentAddArgs {
    /// Agent label to add.
    pub label: String,
    /// Tool this agent runs (`claude` / `codex`). REQUIRED with
    /// `--global` (a library description needs a tool) and to
    /// define a fresh agent inline at repo scope. OMIT at repo
    /// scope to add `<label>` BY NAME — its description is copied
    /// from the user-scope `agents` library.
    #[arg(long, value_enum)]
    pub tool: Option<ToolArg>,
    /// Tier for the new roster entry: `commit` (default), `plan`,
    /// `final`, or `gate`. Ignored with `--global` (the library has
    /// no role).
    #[arg(long, value_enum, default_value = "commit")]
    pub review: ReviewKindArg,
    /// Write the agent DESCRIPTION to the user-scope library
    /// (`~/.clank/config.json#/agents`, reusable, NO role) instead
    /// of the repo roster. Requires `--tool`. No repo change.
    #[arg(long)]
    pub global: bool,
    /// Override the executable used by `clank agent start`. If
    /// unset, the agent's tool name (`claude` / `codex`) is used.
    #[arg(long, value_name = "CMD")]
    pub launch_cmd: Option<String>,
    /// Arguments inserted on the tool invocation BEFORE the
    /// session-restore suffix. Repeat for multiple args.
    /// `allow_hyphen_values` lets you pass values that start with
    /// `-` or `--` directly (e.g. `--launch-arg --profile`).
    #[arg(long = "launch-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub launch_args: Vec<String>,
    /// Environment overrides applied to the spawned process.
    /// Format `KEY=VAL`. Repeatable. `launch.env` wins over the
    /// inherited environment on key collision.
    #[arg(long = "launch-env", value_name = "KEY=VAL")]
    pub launch_envs: Vec<String>,
    /// Initial prompt passed to the resumed tool as a trailing
    /// positional. Pass `""` to explicitly disable the prompt
    /// (escape hatch when `auto_mode == On` but you don't want
    /// the default `Session resumed.` prompt). Unset = follow
    /// auto_mode default.
    #[arg(long, value_name = "STRING")]
    pub initial_prompt: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct AgentPromoteArgs {
    /// Agent label to designate as the repo's master. Must already
    /// be on the roster (`clank agent add <name>` first).
    pub name: String,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct AgentRemoveArgs {
    /// Agent label to remove.
    pub label: String,
    /// Remove the DESCRIPTION from the user-scope library
    /// (`~/.clank/config.json#/agents`) and scrub it from any team
    /// template that references it. Without `--global`, removes the
    /// agent from THIS repo's roster.
    #[arg(long)]
    pub global: bool,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct AgentSetReviewArgs {
    /// Reviewer label whose tier to change.
    pub name: String,
    /// New tier: `commit`, `plan`, `final`, or `gate`.
    #[arg(value_enum)]
    pub review: ReviewKindArg,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

/// `clank team` — GLOBAL roster-template library only
/// (`~/.clank/config.json#/teams`). A "team" is no longer a repo
/// concept: the repo's roster (`clank agent …`) is the operating
/// team. `save` captures the repo roster as a template; `init
/// --team` seeds a repo from one. Plan: `repo-agents-no-team`.
#[derive(Args, Debug)]
pub struct TeamArgs {
    #[command(subcommand)]
    pub command: TeamCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum TeamCmd {
    /// Publish THIS repo's roster as a reusable global template
    /// under `<name>` (user-scope `teams`), copying the roster's
    /// agent definitions into user-scope `agents`. Refuses if a
    /// referenced agent already exists globally with a different
    /// body; pass `--force` to overwrite an existing team name.
    Save(TeamSaveArgs),
    /// List the global team-template library (user-scope).
    List(TeamListArgs),
    /// Show a single global team template (its roster).
    Show(TeamShowArgs),
    /// Delete a global team template. Refuses without `--force`
    /// (clank can't introspect which repos reference it).
    Delete(TeamDeleteArgs),
}

#[derive(Args, Debug)]
pub struct TeamListArgs {
    /// Emit machine-readable JSON instead of human text.
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct TeamShowArgs {
    /// Team template name to show (user-scope `teams`).
    pub team: String,
    /// Emit machine-readable JSON instead of human text.
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct TeamDeleteArgs {
    /// Team name to delete.
    pub team: String,
    /// Delete even if a repo may be seeded from this template
    /// (clank can't introspect that, so this is the explicit user
    /// opt-in).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct TeamSaveArgs {
    /// Name to publish THIS repo's team under in the global
    /// library (`~/.clank/config.json#/teams`).
    pub name: String,
    /// Overwrite an existing user-scope team of the same name.
    /// Does NOT bypass the agent-body collision check — that's a
    /// hard error regardless.
    #[arg(long)]
    pub force: bool,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum ReviewKindArg {
    Commit,
    Plan,
    Final,
    Gate,
}

impl From<ReviewKindArg> for crate::cli::teams_config::ReviewKind {
    fn from(a: ReviewKindArg) -> Self {
        use crate::cli::teams_config::ReviewKind;
        match a {
            ReviewKindArg::Commit => ReviewKind::Commit,
            ReviewKindArg::Plan => ReviewKind::Plan,
            ReviewKindArg::Final => ReviewKind::Final,
            ReviewKindArg::Gate => ReviewKind::Gate,
        }
    }
}

#[derive(clap::Subcommand, Debug)]
pub enum QueueCmd {
    /// Add a queued plan at `.clank/queue/<NNN>-<name>.md`. Body comes
    /// from `.clank/drafts/<name>.md` (write the draft, then `queue add
    /// <name>` consumes it) or `-m "<body>"` for a trivial inline body.
    /// Empty / header-only content is rejected.
    Add(QueueAddArgs),
    /// Remove an item from the queue.
    Remove(QueueRemoveArgs),
    /// Promote a queued item to an active plan.
    Promote(QueuePromoteArgs),
    /// Change a queued item's priority (renames its `NNN-` prefix).
    #[command(alias = "reprioritize")]
    Reprioritise(QueueReprioritiseArgs),
}

#[derive(Args, Debug)]
pub struct PickArgs {
    /// Plan stem(s) to copy from `--from`. Replayed in SOURCE order
    /// (their stacking order over there), regardless of argument order.
    #[arg(required = true)]
    pub plans: Vec<String>,
    /// Branch (or any committish) to copy the plans FROM. Never
    /// modified — `pick` is a copy, not a move.
    #[arg(long, value_name = "COMMITTISH")]
    pub from: String,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Collapse each picked plan's commit stack into ONE commit on
    /// this branch (N plans → N commits, in source order). The commit
    /// message reuses the plan's finalize WHY when it has one.
    #[arg(long)]
    pub squash: bool,
    /// Strip the plan's `.clank/` artifacts from what lands — the code
    /// is copied, the plan file / review scaffolding is not. Commits
    /// that become empty after the strip are dropped. Composes with
    /// --squash (one clean commit).
    #[arg(long)]
    pub purge: bool,
    /// Print the resolved plan order and each plan's commits without
    /// picking anything (the same computation the live run applies).
    #[arg(long)]
    pub dry: bool,
}

#[derive(Args, Debug)]
pub struct QueueAddArgs {
    pub name: String,
    #[arg(long, default_value_t = 500)]
    pub priority: u16,
    /// Inline body. Multi-line allowed (newlines preserved). Omit to
    /// take the body from `.clank/drafts/<name>.md`.
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

#[derive(Args, Debug)]
pub struct QueueRemoveArgs {
    pub name: String,
}

#[derive(Args, Debug)]
pub struct QueuePromoteArgs {
    pub name: String,
}

#[derive(Args, Debug)]
pub struct QueueReprioritiseArgs {
    pub name: String,
    /// New priority, 0-999 (lower = promoted sooner).
    pub priority: u16,
}

#[derive(Args, Debug)]
pub struct FinishArgs {
    /// Plan to finalize. Accepts `<repo>/<stem>.md` or just `<stem>`.
    /// Optional when the cwd-repo has exactly one in-flight active plan.
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// REQUIRED (when authoring the finish commit): the whole plan's commit
    /// message — a brief WHAT subject and a WHY body, written as if the whole
    /// plan were one commit. It becomes the plan's squash summary. A bare
    /// `finish` / subject-only message is rejected. Repeatable, git-style:
    /// each `-m` becomes a paragraph (e.g. `-m "<subject>" -m "<why>"`).
    #[arg(short = 'm', long)]
    pub message: Vec<String>,
    /// After finalize, strip the plan's `.clank/` artifacts
    /// from history (runs the rewrite engine on the just-extended
    /// range). The finalize snapshot is included in the strip.
    #[arg(long)]
    pub purge: bool,
    /// After finalize, collapse the plan's commits into one with
    /// the supplied message. Combine with `--purge` to also strip
    /// the plan's `.clank/` artifacts.
    #[arg(long, value_name = "MSG")]
    pub squash: Option<String>,
    /// Opt OUT of `finish.autosquash` for this one finish — keep the plan's
    /// individual commits (e.g. a bisectable refactor). No effect when
    /// autosquash is off or `--squash` is given explicitly.
    #[arg(long, conflicts_with = "squash")]
    pub no_squash: bool,
    /// Write the rewritten history to a fresh branch instead of
    /// in-place. Only meaningful with `--purge`/`--squash`.
    #[arg(long, value_name = "NAME")]
    pub into_branch: Option<String>,
    /// Preview any history edit this finish would perform, without creating
    /// commits or moving refs: the `--purge`/`--squash` rewrite plan, or the
    /// reword plan for a bare `-m` on an already-finished plan (the same
    /// computation the live run applies). Ignored on a plain fresh finish
    /// (no history rewrite happens there).
    #[arg(long)]
    pub dry: bool,
    /// Finalize even when the review gate hasn't said FINISHED
    /// (unreviewed / continued / changes-requested / pending gate tier).
    /// Bypasses ONLY the review-gate readiness: messages stay mandatory,
    /// autosquash still applies, and safety refusals (dirty plan file,
    /// missing plan file, open human block, no reviewable commit) stand.
    #[arg(long)]
    pub force: bool,
    /// Skip the on-disk state cache: don't read it, don't write it.
    #[arg(long)]
    pub no_cache: bool,
}

#[derive(Args, Debug)]
/// Bare `clank stash [PLAN]` IS a push, as `git stash` is — the
/// verbs are for the rest (stash-does-what-you-mean).
///
/// The push options live on the parent for the bare form only: with
/// a verb present they are refused by [`crate::cli::stash::run`]
/// rather than parsed and dropped — `clank stash --dry drop foo` must
/// not run a drop the operator asked to preview (codex on 70a17f8).
/// Each verb carries its own `--repo`. A verb's name always means the
/// verb, even after an option (`subcommand_precedence_over_arg`); a
/// plan called `list` is stashed with `push list`. clap's own
/// `args_conflicts_with_subcommands` is NOT the tool: with a parent
/// positional it stops recognising the verb once an option was seen,
/// so `--repo /x list` would stash a plan named `list`.
#[command(subcommand_precedence_over_arg = true)]
pub struct StashArgs {
    #[command(subcommand)]
    pub command: Option<StashCmd>,
    #[command(flatten)]
    pub push: StashPushArgs,
}

#[derive(clap::Subcommand, Debug)]
pub enum StashCmd {
    /// Set a plan's commits aside (kept restorable on a protective ref)
    /// and clean them off the branch — the same as bare `clank stash`.
    Push(StashPushArgs),
    /// Restore a stashed plan: replay its commits onto HEAD and consume
    /// the stash. Reviews reset (the replayed commits are re-reviewed).
    Pop(StashPopArgs),
    /// Print a stashed plan's body (from its protective ref) + commits.
    Show(StashShowArgs),
    /// Permanently discard a plan's stashed commits + record.
    Drop(StashDropArgs),
    /// List the stashed plans.
    List(StashListArgs),
}

#[derive(Args, Debug)]
pub struct StashPopArgs {
    /// Stashed plan stem to restore. Optional when exactly one plan is
    /// stashed.
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StashShowArgs {
    /// Stashed plan stem. Optional when exactly one plan is stashed.
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StashDropArgs {
    /// Stashed plan stem. Optional when exactly one plan is stashed.
    pub plan: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StashListArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StashPushArgs {
    /// Plan stem to stash (same parsing as `clank purge`). Optional when
    /// the repo has exactly one in-flight active plan.
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Record that this plan is waiting on another; `clank status`
    /// nudges to pop once that plan finishes.
    #[arg(long = "for", value_name = "PLAN")]
    pub waiting_for: Option<String>,
    /// Also save the plan body back to the queue for re-attempt
    /// from scratch.
    #[arg(long)]
    pub to_queue: bool,
    /// Queue priority for `--to-queue` (default 500).
    #[arg(long, value_name = "N")]
    pub priority: Option<u16>,
    /// Print the plan without changing anything.
    #[arg(long)]
    pub dry: bool,
}

/// HIDDEN ALIAS (one release): `clank shelve` → `clank stash push`,
/// `clank shelve clean` → `clank stash drop`.
#[derive(Args, Debug)]
#[command(subcommand_precedence_over_arg = true)]
pub struct ShelveArgs {
    #[command(subcommand)]
    pub command: Option<ShelveCmd>,
    /// Plan stem to shelve (same parsing as `clank purge`).
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Record that this plan is waiting on another; `clank status`
    /// nudges to pop once that plan finishes.
    #[arg(long = "for", value_name = "PLAN")]
    pub waiting_for: Option<String>,
    /// Also save the plan body back to the queue for re-attempt
    /// from scratch (absorbs the removed `clank demote`).
    #[arg(long)]
    pub to_queue: bool,
    /// Queue priority for `--to-queue` (default 500).
    #[arg(long, value_name = "N")]
    pub priority: Option<u16>,
    /// Print the plan without changing anything.
    #[arg(long)]
    pub dry: bool,
}

#[derive(clap::Subcommand, Debug)]
pub enum ShelveCmd {
    /// Permanently discard a plan's shelved commits + state.
    Clean(ShelveCleanArgs),
}

#[derive(Args, Debug)]
pub struct ShelveCleanArgs {
    /// Shelved plan stem. Optional when exactly one plan is stashed.
    pub plan: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct UnshelveArgs {
    /// Shelved plan stem to restore. Optional when exactly one plan is
    /// stashed.
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct UnfinishArgs {
    /// Plan to unfinish. Required.
    pub plan: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct BlockArgs {
    #[command(subcommand)]
    pub command: BlockCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum BlockCmd {
    /// Create a block.
    Create(BlockCreateArgs),
    /// Remove answered block+unblock pairs for this agent.
    Clean(BlockCleanArgs),
    /// Flatten legacy plan-scoped blocks to the repo-wide layout.
    Migrate(BlockMigrateArgs),
}

#[derive(Args, Debug)]
pub struct BlockMigrateArgs {
    /// Print what would change without touching anything.
    #[arg(long)]
    pub dry: bool,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct BlockCreateArgs {
    /// Block name.
    pub name: String,
    /// Question or reason for the block.
    #[arg(short = 'm', value_name = "MSG")]
    pub message: String,
    /// Agent label. Resolved from env when omitted.
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct BlockCleanArgs {
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// TWO WORDS for what you are waiting on — `--desc "test run"`.
    ///
    /// Required, and load-bearing twice over: it is what
    /// `clank status --tui` shows, AND it is how the Stop hook
    /// recognises this run among the tool's live background tasks.
    /// The hook reads the command line the harness recorded, so the
    /// description has to be ON it — which is why clank cannot just
    /// generate an id for you.
    #[arg(long, value_name = "TEXT")]
    pub desc: String,
    /// Agent label. Resolved from env when omitted.
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// The command to run, after `--`.
    #[arg(trailing_var_arg = true, required = true, value_name = "CMD")]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct AttendingArgs {
    /// The background task id you are waiting on, as the tool reports
    /// it. Omit with `--clear` to stop attending.
    pub task_id: Option<String>,
    /// PID of the process you are waiting on, so `clank status` can
    /// tell a live wait from one that already ended. A task handle
    /// carries no pid: have the backgrounded command record its own
    /// (`echo $$ > .clank/agents/<label>/attending.pid`) and pass it
    /// here. Optional — without it the wait is recorded but cannot be
    /// shown as live or ended.
    #[arg(long, value_name = "PID")]
    pub pid: Option<i32>,
    /// TWO WORDS for what you are waiting on — `--desc "test run"`.
    /// Required: this is what `clank status --tui` shows, and a wait
    /// nobody can identify is worse than no wait at all. Longer text
    /// is truncated to fit, never rejected.
    #[arg(long, value_name = "TEXT", required_unless_present = "clear")]
    pub desc: Option<String>,
    /// Forget the recorded task.
    #[arg(long)]
    pub clear: bool,
    /// Agent label. Resolved from env when omitted.
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct UnblockArgs {
    /// Agent that is blocked.
    pub agent: String,
    /// Block name to answer.
    pub name: String,
    /// Answer message.
    #[arg(short = 'm', value_name = "MSG")]
    pub message: String,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct PurgeArgs {
    /// Plan to purge. Accepts `<repo>/<stem>.md` or just `<stem>`.
    /// Omit to infer the single active in-flight plan, or pass
    /// `--all` to strip every `.clank/` path. Mutually exclusive
    /// with `--all`.
    pub plan: Option<String>,
    /// Strip EVERY `.clank/` path from history (plan files,
    /// finalize snapshots, AND non-plan Clank metadata like
    /// `.clank/.gitignore` and `.clank/queue/*`). Cannot be
    /// combined with a plan argument.
    #[arg(long)]
    pub all: bool,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Write the rewritten chain to a fresh branch instead of
    /// rewriting the current branch in place. Safer — the operator
    /// can inspect / cherry-pick / diff before deciding what to do
    /// with it. Refuses if `<name>` already exists.
    #[arg(long, value_name = "NAME")]
    pub into_branch: Option<String>,
    /// Print the planned rewrite and exit 0 without creating any
    /// commits or moving any refs.
    #[arg(long)]
    pub dry: bool,
    /// Collapse the plan-attributed range into a single commit
    /// with the supplied message. Not pipeable to `git rebase` —
    /// see `--dry` output for the planned target tree.
    #[arg(long, value_name = "MSG")]
    pub squash: Option<String>,
    /// Amend HEAD instead of building a new chain. HEAD must
    /// already be a finalize commit (paths under `.clank/finished/`
    /// and `.clank/plans/` matching the plan stem, or under
    /// `.clank/finished/` for `--all`).
    #[arg(long)]
    pub amend: bool,
    /// Skip the on-disk state cache: don't read it, don't write it.
    #[arg(long)]
    pub no_cache: bool,
    /// Drop EVERY commit attributed to the plan, including the
    /// implementation code — not just `.clank/` artifacts. The
    /// plan AND its work both vanish from history. Refuses
    /// foreign commits unconditionally (same policy as
    /// `clank stash push --to-queue`). Does NOT save the plan body —
    /// use `clank stash push --to-queue` if you want to re-queue it for
    /// another attempt. Mutually exclusive with `--all`,
    /// `--squash`, and `--amend`.
    #[arg(long)]
    pub drop: bool,
}

/// Resolve the repo root: explicit `--repo` path wins, otherwise
/// `git rev-parse --show-toplevel` from cwd. Both branches go
/// through `dunce::canonicalize` so macOS `/var → /private/var`
/// symlinks don't yield divergent identities.
pub(crate) fn resolve_repo(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    let raw = if let Some(p) = explicit {
        p.to_path_buf()
    } else {
        let cwd = std::env::current_dir()?;
        crate::git_io::discover_work_dir(&cwd)?.ok_or_else(|| {
            anyhow::anyhow!(
                "no --repo given and the working directory is not inside a git repository"
            )
        })?
    };
    Ok(dunce::canonicalize(&raw)?)
}

/// Derive the repo basename — the segment Clank uses to address
/// plans as `<basename>/<stem>.md` (e.g. in CLI args and error
/// messages). Wraps `RepoBasename::from_repo_root` so the
/// validation rule lives in one place.
pub(crate) fn repo_basename(repo: &Path) -> anyhow::Result<String> {
    clank_core::ids::RepoBasename::from_repo_root(repo)
        .map(|b| b.as_str().to_string())
        .ok_or_else(|| anyhow::anyhow!("repo path has no usable basename: {}", repo.display()))
}

#[cfg(test)]
mod stash_cli_parse_tests {
    use super::{StashArgs, StashCmd};
    use crate::cli::command::{Cli, Command};
    use clap::Parser;

    /// Through the PRODUCTION tree, `clank stash …`: the settings that
    /// decide verb-vs-positional live on the `stash` command clap
    /// builds from `StashArgs`, and a flattened test root is not it.
    fn parse(argv: &[&str]) -> Result<StashArgs, clap::Error> {
        Cli::try_parse_from(["clank", "stash"].into_iter().chain(argv.iter().copied())).map(|cli| {
            match cli.command {
                Command::Stash(a) => a,
                _ => unreachable!("parsed under `stash`"),
            }
        })
    }

    /// A parent option before a verb is refused, never parsed and
    /// dropped: `--dry` before `drop` would otherwise run the drop
    /// (codex on 70a17f8). clap recognises the verb (it is not swallowed
    /// as the plan) and `stash::run` refuses the combination before it
    /// touches anything.
    #[test]
    fn parent_options_are_refused_when_a_verb_is_present() {
        use crate::cli::stash::refuse_parent_options;
        for argv in [
            &["--dry", "drop", "foo"][..],
            &["--repo", "/x", "list"],
            &["--repo", "/x", "drop", "foo"],
            &["foo", "list"],
            &["--to-queue", "pop"],
        ] {
            let parsed = parse(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(parsed.command.is_some(), "{argv:?}: the verb is the verb");
            let err = refuse_parent_options(&parsed)
                .expect_err(&format!("{argv:?} must be refused"))
                .to_string();
            assert!(err.contains("after the verb"), "{argv:?}: {err}");
        }
        // Nothing before the verb: fine.
        refuse_parent_options(&parse(&["list"]).unwrap()).unwrap();
        refuse_parent_options(&parse(&["drop", "foo", "--repo", "/x"]).unwrap()).unwrap();
        // And the bare form is never refused for carrying its own options.
        refuse_parent_options(&parse(&["foo", "--dry", "--repo", "/x"]).unwrap()).unwrap();
    }

    /// The hidden aliases are the same command in old clothes: parsed
    /// through the production tree, converted, and refused or inferred
    /// exactly as `stash` is (codex on c7cf432).
    #[test]
    fn the_shelve_and_unshelve_aliases_are_stash_args() {
        use crate::cli::stash::{refuse_parent_options, shelve_as_stash, unshelve_as_stash};
        let shelve = |argv: &[&str]| {
            Cli::try_parse_from(["clank", "shelve"].into_iter().chain(argv.iter().copied())).map(
                |cli| match cli.command {
                    Command::Shelve(a) => shelve_as_stash(a),
                    _ => unreachable!(),
                },
            )
        };
        // `shelve --dry clean foo`: the verb is the verb, and the stray
        // option is refused.
        let a = shelve(&["--dry", "clean", "foo", "--repo", "/x"]).unwrap();
        assert!(matches!(a.command, Some(StashCmd::Drop(_))));
        assert!(
            refuse_parent_options(&a)
                .unwrap_err()
                .to_string()
                .contains("--dry")
        );
        // `shelve clean` with no plan infers, like `stash drop`.
        match shelve(&["clean"]).unwrap().command {
            Some(StashCmd::Drop(d)) => assert!(d.plan.is_none()),
            other => panic!("{other:?}"),
        }
        // Bare `shelve foo --to-queue` is the bare push.
        let a = shelve(&["foo", "--to-queue"]).unwrap();
        assert!(a.command.is_none() && a.push.to_queue && a.push.plan.as_deref() == Some("foo"));
        refuse_parent_options(&a).unwrap();

        // `unshelve [plan]` is `stash pop [plan]`.
        let unshelve = |argv: &[&str]| {
            Cli::try_parse_from(
                ["clank", "unshelve"]
                    .into_iter()
                    .chain(argv.iter().copied()),
            )
            .map(|cli| match cli.command {
                Command::Unshelve(a) => unshelve_as_stash(a),
                _ => unreachable!(),
            })
        };
        match unshelve(&[]).unwrap().command {
            Some(StashCmd::Pop(p)) => assert!(p.plan.is_none()),
            other => panic!("{other:?}"),
        }
        match unshelve(&["foo", "--repo", "/x"]).unwrap().command {
            Some(StashCmd::Pop(p)) => {
                assert_eq!(p.plan.as_deref(), Some("foo"));
                assert_eq!(p.repo.as_deref(), Some(std::path::Path::new("/x")));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_bare_form_carries_the_push_options_and_each_verb_its_own_repo() {
        let bare = parse(&["foo", "--dry", "--repo", "/x"]).unwrap();
        assert!(bare.command.is_none());
        assert_eq!(bare.push.plan.as_deref(), Some("foo"));
        assert!(bare.push.dry);
        assert_eq!(bare.push.repo.as_deref(), Some(std::path::Path::new("/x")));

        let bare = parse(&[]).unwrap();
        assert!(bare.command.is_none() && bare.push.plan.is_none());
        // A verb's name is the verb, wherever it sits.
        assert!(matches!(
            parse(&["list"]).unwrap().command,
            Some(StashCmd::List(_))
        ));

        match parse(&["list", "--repo", "/x"]).unwrap().command {
            Some(StashCmd::List(a)) => {
                assert_eq!(a.repo.as_deref(), Some(std::path::Path::new("/x")))
            }
            other => panic!("expected list, got {other:?}"),
        }
        match parse(&["drop", "--repo", "/x"]).unwrap().command {
            Some(StashCmd::Drop(a)) => {
                assert!(a.plan.is_none(), "drop infers the one stash");
                assert_eq!(a.repo.as_deref(), Some(std::path::Path::new("/x")));
            }
            other => panic!("expected drop, got {other:?}"),
        }
        match parse(&["push", "foo", "--dry"]).unwrap().command {
            Some(StashCmd::Push(a)) => assert!(a.dry && a.plan.as_deref() == Some("foo")),
            other => panic!("expected push, got {other:?}"),
        }
    }
}

mod agent_team_cli_parse_tests {
    use super::{AgentArgs, ForkCreateArgs, TeamArgs};
    use clap::Parser;

    // `AgentArgs` / `TeamArgs` derive `Args` (they wrap a
    // subcommand), so flatten them under a test root and drop the
    // `agent` / `team` prefix from the argv.
    #[derive(Parser)]
    struct AgentT {
        #[command(flatten)]
        args: AgentArgs,
    }

    #[derive(Parser)]
    struct TeamT {
        #[command(flatten)]
        args: TeamArgs,
    }

    #[derive(Parser)]
    struct ForkT {
        #[command(flatten)]
        args: ForkCreateArgs,
    }

    #[derive(Parser)]
    struct ForkNoun {
        #[command(subcommand)]
        cmd: super::ForkCmd,
    }

    #[test]
    fn a_bare_token_can_never_be_read_as_a_fork_name() {
        // THE regression. `clank fork list` used to create a worktree,
        // a branch and forked team sessions called `list` — a silently
        // successful wrong action. No bare token parses at all now.
        assert!(
            ForkNoun::try_parse_from(["t", "list-forks-please"]).is_err(),
            "a bare name must not parse as creation"
        );
        assert!(
            matches!(
                ForkNoun::try_parse_from(["t", "list"]).unwrap().cmd,
                super::ForkCmd::List(_)
            ),
            "`list` is the subcommand"
        );
    }

    #[test]
    fn list_is_still_usable_as_a_name_when_stated_explicitly() {
        // The invariant is about AMBIGUITY, not about banning the word.
        let parsed = ForkNoun::try_parse_from(["t", "create", "list"]).unwrap();
        match parsed.cmd {
            super::ForkCmd::Create(a) => assert_eq!(a.name.as_deref(), Some("list")),
            other => panic!("expected create, got {other:?}"),
        }
    }

    #[test]
    fn create_keeps_the_pr_derived_name_rule() {
        // `--pr N` derives `pr-<N>`, so NAME is required-unless-pr —
        // the contract the noun grammar must not silently drop.
        assert!(
            ForkNoun::try_parse_from(["t", "create", "--pr", "7"]).is_ok(),
            "--pr derives the name"
        );
        assert!(
            ForkNoun::try_parse_from(["t", "create"]).is_err(),
            "without a name or --pr there is nothing to create"
        );
    }

    #[test]
    fn fork_draft_repeats_with_short_and_long_flags_in_order() {
        let t = ForkT::try_parse_from(["t", "wt", "-d", "a", "-d", "b", "--draft", "c"]).unwrap();
        assert_eq!(t.args.drafts, vec!["a", "b", "c"], "list order preserved");
    }

    #[test]
    fn agent_add_tool_is_optional_for_by_name() {
        // repo-agents-no-team: `agent add <name>` BY NAME (no
        // `--tool`) is valid (copy-down from the library).
        assert!(
            AgentT::try_parse_from(["t", "add", "claude"]).is_ok(),
            "agent add by-name (no --tool) must parse"
        );
        assert!(
            AgentT::try_parse_from(["t", "add", "claude", "--tool", "claude"]).is_ok(),
            "agent add --tool claude must parse"
        );
    }

    #[test]
    fn agent_add_accepts_review_flag() {
        // `--review` is back on `agent add` (one-step define + role).
        assert!(
            AgentT::try_parse_from(["t", "add", "claude", "--tool", "claude", "--review", "gate"])
                .is_ok(),
            "agent add --review gate must parse"
        );
        assert!(
            AgentT::try_parse_from(["t", "add", "claude", "--review", "commit"]).is_ok(),
            "agent add --review commit (by name) must parse"
        );
    }

    #[test]
    fn agent_promote_parses() {
        assert!(
            AgentT::try_parse_from(["t", "promote", "codex"]).is_ok(),
            "agent promote <name> must parse"
        );
        // The `set-master` alias was DROPPED — it must no longer parse
        // (regression guard, inverted from the old back-compat assertion).
        assert!(
            AgentT::try_parse_from(["t", "set-master", "codex"]).is_err(),
            "agent set-master alias must no longer parse (dropped)"
        );
    }

    #[test]
    fn agent_set_review_parses() {
        assert!(
            AgentT::try_parse_from(["t", "set-review", "codex", "gate"]).is_ok(),
            "agent set-review <name> <tier> must parse"
        );
        assert!(
            AgentT::try_parse_from(["t", "set-review", "codex", "commit"]).is_ok(),
            "commit tier must parse"
        );
        // Bad tier is rejected by the value-enum.
        assert!(
            AgentT::try_parse_from(["t", "set-review", "codex", "nope"]).is_err(),
            "invalid tier must be rejected"
        );
    }

    #[test]
    fn agent_global_requires_tool_handled_at_runtime() {
        // `--global` without `--tool` PARSES (the requirement is
        // enforced at runtime, not by clap, so the by-name path can
        // omit --tool).
        assert!(AgentT::try_parse_from(["t", "add", "claude", "--global"]).is_ok());
        assert!(
            AgentT::try_parse_from(["t", "add", "claude", "--global", "--tool", "claude"]).is_ok()
        );
    }

    #[test]
    fn team_repo_subcommands_are_removed() {
        // repo-agents-no-team: team {add,remove,set-master,show <none>}
        // are gone (they moved to `clank agent …`).
        assert!(
            TeamT::try_parse_from(["t", "add", "codex"]).is_err(),
            "team add must no longer be a subcommand"
        );
        assert!(
            TeamT::try_parse_from(["t", "remove", "codex"]).is_err(),
            "team remove must no longer be a subcommand"
        );
        assert!(
            TeamT::try_parse_from(["t", "set-master", "codex"]).is_err(),
            "team set-master must no longer be a subcommand"
        );
        // `team show` now REQUIRES a template name (global only).
        assert!(
            TeamT::try_parse_from(["t", "show"]).is_err(),
            "team show now requires a <name>"
        );
        assert!(TeamT::try_parse_from(["t", "show", "dev"]).is_ok());
    }

    #[test]
    fn team_create_is_removed() {
        assert!(
            TeamT::try_parse_from(["t", "create", "dev"]).is_err(),
            "team create must no longer be a subcommand"
        );
    }

    #[test]
    fn team_library_subcommands_still_parse() {
        assert!(TeamT::try_parse_from(["t", "save", "dev"]).is_ok());
        assert!(TeamT::try_parse_from(["t", "save", "dev", "--force"]).is_ok());
        assert!(TeamT::try_parse_from(["t", "list"]).is_ok());
        assert!(TeamT::try_parse_from(["t", "delete", "dev"]).is_ok());
        assert!(TeamT::try_parse_from(["t", "delete", "dev", "--force"]).is_ok());
    }
}
