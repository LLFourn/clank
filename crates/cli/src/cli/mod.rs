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
pub mod config;
pub mod feedback;
pub mod finish;
pub mod init;
pub mod plan_resolve;
pub mod purge;
pub mod rewrite;
pub mod status;
pub mod wfw;

#[derive(Args, Debug)]
pub struct InitArgs {
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
    /// Render every active plan instead of inferring a single one.
    /// Mutually exclusive with `--plan`.
    #[arg(long, conflicts_with = "plan")]
    pub all: bool,
    /// Specific plan to render. Accepts `<stem>`, `<stem>.md`, or
    /// `<basename>/<stem>.md` — same parser as `clank finish`.
    #[arg(long, value_name = "PLAN")]
    pub plan: Option<String>,
}

#[derive(Args, Debug)]
pub struct WfwArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Required: the agent label this caller is wfw-ing as. Keys
    /// feedback files and the participant set; pick something stable
    /// across this agent's sessions (`claude`, `claude-fe`, etc.).
    #[arg(long, value_name = "LABEL")]
    pub author: String,
    /// Required: which side of the workflow this caller plays.
    #[arg(long, value_enum)]
    pub role: WfwRole,
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
    /// Write a feedback file: validates the body's first
    /// non-blank line matches `--verdict`, resolves `--commit`
    /// against the plan's reviewable shas, and writes
    /// `.clank/agents/<author>/feedback/<plan>/<stem>.md`
    /// atomically.
    Write(FeedbackWriteArgs),
}

#[derive(Args, Debug)]
pub struct FeedbackWriteArgs {
    /// Repo root. Defaults to the cwd's git toplevel.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Plan stem (e.g. `clank-agent-integration`). Same parser as
    /// `clank finish`/`clank status` — `<stem>`, `<stem>.md`, or
    /// `<basename>/<stem>.md`.
    #[arg(long, value_name = "PLAN")]
    pub plan: String,
    /// Commit ref (7+ lowercase hex chars). Resolved against the
    /// plan's reviewable commits; ambiguous prefixes are an error.
    #[arg(long, value_name = "SHA")]
    pub commit: String,
    /// Verdict claim. Must match the body's first non-blank line
    /// (`APPROVE` or `REQUEST_CHANGES`); mismatch is an error.
    #[arg(long, value_enum)]
    pub verdict: VerdictArg,
    /// Agent label to attribute the feedback to. Required for
    /// now; will become optional when the identity resolver lands
    /// (then defaults via `CLAUDE_CODE_SESSION_ID` /
    /// `CODEX_THREAD_ID`).
    #[arg(long, value_name = "LABEL")]
    pub author: String,
    /// Read body from this file. `-` (default) reads from stdin.
    #[arg(long, value_name = "PATH", default_value = "-")]
    pub body_file: String,
}

/// Verdict the writer is claiming for this feedback. Maps to
/// [`clank_core::Verdict`] for validation. `Unmarked` is
/// intentionally not selectable — every written file carries a
/// real verdict.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum VerdictArg {
    Approve,
    RequestChanges,
}

impl From<VerdictArg> for clank_core::Verdict {
    fn from(v: VerdictArg) -> Self {
        match v {
            VerdictArg::Approve => clank_core::Verdict::Approve,
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
    /// Enable auto-mode (hint by default; opt into blocking
    /// long-poll with `--mode wait`). Optionally update role.
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
    /// Auto-mode flavor. `hint` (default) does a cheap status
    /// check and emits a continuation only when work is already
    /// pending; `wait` long-polls `clank wfw` and blocks the
    /// agent until work arrives or `wfw_timeout` elapses.
    #[arg(long, value_enum, default_value_t = AutoModeArg::Hint)]
    pub mode: AutoModeArg,
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

/// Auto-mode flavor for `clank auto on --mode`. Maps to
/// [`clank_core::AutoMode`]; the `Off` value is intentionally
/// not selectable here (that's what `clank auto off` is for).
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum AutoModeArg {
    Hint,
    Wait,
}

/// Role designation for `--role`.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum RoleArg {
    Master,
    Reviewers,
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
    /// Override the default `Finalize <stem>` commit message.
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
pub struct PurgeArgs {
    /// Plan to purge. Accepts `<repo>/<stem>.md` or just `<stem>`.
    /// Omit to infer the single active in-flight plan, or pass
    /// `--all` to strip every `.clank/` path. Mutually exclusive
    /// with `--all`.
    pub plan: Option<String>,
    /// Strip EVERY `.clank/` path from history (plan files,
    /// finalize snapshots, AND non-plan Clank metadata like
    /// `.clank/.gitignore` and `.clank/stubs/*`). Cannot be
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
    /// already be a finalize commit (every changed path under
    /// `.clank/finished/<stem>/`, or under `.clank/finished/`
    /// for `--all`).
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
