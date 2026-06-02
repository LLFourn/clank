//! Operator-facing CLI commands. Clank is daemonless: each
//! subcommand folds the cwd-repo locally (via the sans-io fold in
//! `clank-core::repo_state`), projects a typed preview (see
//! `crate::preview`), and only then performs mutations via local
//! `git` subprocess calls and direct filesystem writes. The
//! preview is the contract every mutation runs against — it makes
//! "what would happen" inspectable before "do it" runs.

use clap::Args;
use std::path::{Path, PathBuf};

pub mod as_cmd;
pub mod auto;
pub mod block;
pub mod config;
pub mod doctor;
pub mod feedback;
pub mod finish;
pub mod init;
pub mod log;
pub mod open;
pub mod plan_resolve;
pub mod rewire;
pub mod purge;
pub mod rewrite;
pub mod queue;
pub mod setup;
pub mod status;
pub mod stop_hook;
pub mod unfinish;
pub mod wfw;

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigKey>,
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    #[arg(short = 'j', long)]
    pub json: bool,
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
    #[command(name = "review.require_commit_prefix", alias = "review_require_commit_prefix")]
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
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Skip the interactive agent-identity prompts (phase 2)
    /// and accept defaults: label = tool name (claude/codex),
    /// role = reviewers. Useful for scripts.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Overwrite an existing foreign `post-rewrite` hook.
    #[arg(long)]
    pub force_hooks: bool,
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
}

#[derive(Args, Debug)]
pub struct OpenArgs {
    /// Path to inspect. Echoed back verbatim as `requested_path`;
    /// not canonicalized until existence is confirmed.
    pub path: String,
    /// Emit the response as JSON.
    #[arg(short = 'j', long)]
    pub json: bool,
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
}

#[derive(Args, Debug)]
pub struct WfwArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Agent label this caller is wfw-ing as. Keys feedback files
    /// and the participant set. Optional: defaults via the shared
    /// identity resolver (CLANK_AGENT env > session lookup via
    /// CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID). Pass explicitly
    /// to override or to run from outside a session.
    #[arg(long, value_name = "LABEL")]
    pub author: Option<String>,
    /// Which side of the workflow this caller plays. Optional:
    /// defaults to `master` iff the resolved label matches
    /// `.clank/config.json`'s `master` field, else `reviewers`.
    #[arg(long, value_enum)]
    pub role: Option<WfwRole>,
    /// Restrict watch / report to one plan (same parser as
    /// `clank status --plan`). Without it, wfw considers every
    /// active plan in the repo.
    #[arg(long, value_name = "PLAN")]
    pub plan: Option<String>,
    /// Maximum wait. Accepts `30s`, `5m`, `1h`. `0` (default) means
    /// wait indefinitely.
    #[arg(long, default_value = "0", value_name = "DURATION")]
    pub timeout: String,
    /// Emit JSON instead of the human rendering.
    #[arg(short = 'j', long)]
    pub json: bool,
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

impl WfwArgs {
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

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum WfwRole {
    Master,
    Reviewers,
}

impl From<WfwRole> for clank_core::Role {
    fn from(r: WfwRole) -> Self {
        match r {
            WfwRole::Master => clank_core::Role::Master,
            WfwRole::Reviewers => clank_core::Role::Reviewers,
        }
    }
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
    /// Verdict. Prepended to the body as `APPROVE <body>` or
    /// `REQUEST_CHANGES <body>`.
    #[arg(long, value_enum)]
    pub verdict: VerdictArg,
    /// Agent label to attribute the feedback to.
    #[arg(long, value_name = "LABEL")]
    pub author: String,
    /// Review message (like `git commit -m`). First line is the
    /// summary; subsequent lines are details. Required.
    #[arg(short = 'm', value_name = "MSG")]
    pub message: String,
}

/// Verdict the writer is claiming for this feedback. Maps to
/// [`clank_core::Verdict`] for validation. `Unmarked` is
/// intentionally not selectable — every written file carries a
/// real verdict.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum VerdictArg {
    Approve,
    Finished,
    RequestChanges,
}

impl From<VerdictArg> for clank_core::Verdict {
    fn from(v: VerdictArg) -> Self {
        match v {
            VerdictArg::Approve => clank_core::Verdict::Approve,
            VerdictArg::Finished => clank_core::Verdict::Finished,
            VerdictArg::RequestChanges => clank_core::Verdict::RequestChanges,
        }
    }
}

#[derive(Args, Debug)]
pub struct AsArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// The agent label to bind this session to. Subsequent calls
    /// to `clank wfw` / `clank auto` / the stop-hook resolve to
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
    /// Designate this agent as `master` or `reviewers` for the
    /// repo. `master` writes `.clank/config.json` with this
    /// label; `reviewers` clears master only if this agent
    /// currently holds it.
    #[arg(long, value_enum)]
    pub role: Option<RoleArg>,
    /// Override the wait-for-work timeout (e.g. `30m`, `5m`,
    /// `45s`). Omit to leave unchanged; `null` in the underlying
    /// config means "indefinite".
    #[arg(long, value_name = "DUR")]
    pub wfw_timeout: Option<String>,
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
    Reviewers,
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
}

/// `--tool` value. Mirrors [`clank_core::Tool`].
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum ToolArg {
    Claude,
    Codex,
}

impl From<ToolArg> for clank_core::Tool {
    fn from(t: ToolArg) -> Self {
        match t {
            ToolArg::Claude => clank_core::Tool::Claude,
            ToolArg::Codex => clank_core::Tool::Codex,
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

#[derive(clap::Subcommand, Debug)]
pub enum QueueCmd {
    /// Create an empty queued plan stub at
    /// `.clank/queue/<NNN>-<name>.md`. The agent (or human)
    /// then edits that file in place.
    Add(QueueAddArgs),
    /// Remove an item from the queue.
    Remove(QueueRemoveArgs),
    /// Promote a queued item to an active plan.
    Promote(QueuePromoteArgs),
}

#[derive(Args, Debug)]
pub struct QueueAddArgs {
    pub name: String,
    #[arg(long, default_value_t = 500)]
    pub priority: u16,
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
pub struct FinishArgs {
    /// Plan to finalize. Accepts `<repo>/<stem>.md` or just `<stem>`.
    /// Optional when the cwd-repo has exactly one in-flight active plan.
    pub plan: Option<String>,
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Amend HEAD instead of creating a new finalize commit. HEAD
    /// must already be a finalize commit for this plan.
    #[arg(long)]
    pub amend: bool,
    /// Override the default `[<stem>] finish` commit message.
    #[arg(short = 'm', long)]
    pub message: Option<String>,
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
    /// Write the rewritten history to a fresh branch instead of
    /// in-place. Only meaningful with `--purge`/`--squash`.
    #[arg(long, value_name = "NAME")]
    pub into_branch: Option<String>,
    /// Permit rewriting a protected branch (`main`/`master`) when
    /// combined with `--purge`/`--squash`.
    #[arg(long)]
    pub allow_rewrite_protected: bool,
    /// Dry-run for `--purge`/`--squash`: emit the rebase-todo
    /// without creating the finalize commit or moving any refs.
    /// Ignored on plain `clank finish`.
    #[arg(long)]
    pub dry: bool,
    /// Skip the on-disk state cache: don't read it, don't write it.
    #[arg(long)]
    pub no_cache: bool,
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
}

#[derive(Args, Debug)]
pub struct BlockCreateArgs {
    /// Block name.
    pub name: String,
    /// Question or reason for the block.
    #[arg(short = 'm', value_name = "MSG")]
    pub message: String,
    /// Scope block to a specific plan.
    #[arg(long, value_name = "PLAN")]
    pub plan: Option<String>,
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
pub struct UnblockArgs {
    /// Agent that is blocked.
    pub agent: String,
    /// Block name to answer.
    pub name: String,
    /// Answer message.
    #[arg(short = 'm', value_name = "MSG")]
    pub message: String,
    /// Plan scope (must match the block's scope).
    #[arg(long, value_name = "PLAN")]
    pub plan: Option<String>,
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
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
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
    /// Permit rewriting a protected branch (`main`/`master` or any
    /// branch matched by `branch.<name>.protect` in git config).
    /// Without this flag, the engine refuses to rewrite a
    /// protected branch in place. `--into-branch` bypasses the
    /// protection check because it doesn't touch the protected
    /// branch.
    #[arg(long)]
    pub allow_rewrite_protected: bool,
    /// Skip the on-disk state cache: don't read it, don't write it.
    #[arg(long)]
    pub no_cache: bool,
}

/// Resolve the repo root: explicit `--repo` path wins, otherwise
/// `git rev-parse --show-toplevel` from cwd. Both branches go
/// through `dunce::canonicalize` so macOS `/var → /private/var`
/// symlinks don't yield divergent identities.
pub(crate) fn resolve_repo(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    let raw = if let Some(p) = explicit {
        p.to_path_buf()
    } else {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "no --repo given and `git rev-parse --show-toplevel` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let root = String::from_utf8(output.stdout)?.trim().to_string();
        PathBuf::from(root)
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
