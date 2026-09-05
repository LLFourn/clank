//! `clank agent` — the repo's ROSTER plus the user-scope agent
//! library (`repo-agents-no-team`).
//!
//! The repo config's `agents` IS the operating roster: a flat
//! `BTreeMap<AgentLabel, RosterAgent>` where each entry carries
//! its definition (tool / launch / initial_prompt) AND its role
//! (master / commit / gate). There is no separate `team`.
//! `agent add` is ONE step (definition + role); `agent promote`
//! picks the master; the ROLE RESOLVER
//! (`agent_store::try_resolve_via_team`) reads the roster directly.
//! The per-agent skeleton at `.clank/agents/<label>/config.json`
//! holds ONLY per-machine state (`auto_mode`,
//! `session`). `list` joins the resolved set with each agent's
//! skeleton session.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;

use crate::agent_store::load_agent_config;
use crate::cli::session_holder::{self, Holder};
use crate::cli::teams_config::{
    AgentDescription, RepoConfigFile, ReviewKind, RosterAgent, RosterRole, UserConfigFile,
};
use clank_core::agent_config::{LaunchConfig, Session};
use clank_core::ids::AgentLabel;
use clank_core::vocab::{AutoMode, Tool};
use std::collections::{BTreeMap, BTreeSet};

use super::{
    AgentAddArgs, AgentArgs, AgentCmd, AgentListArgs, AgentPromoteArgs, AgentRemoveArgs,
    AgentSetReviewArgs, AgentStartArgs, AgentSwapArgs, resolve_repo,
};
use crate::shell_quote::shell_quote;

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        AgentCmd::List(a) => list(a),
        AgentCmd::Start(a) => start(a),
        AgentCmd::Add(a) => add(a),
        AgentCmd::Promote(a) => promote(a),
        AgentCmd::Remove(a) => remove(a),
        AgentCmd::SetReview(a) => set_review(a),
        AgentCmd::Swap(a) => swap(a),
    }
}

#[derive(Debug, Serialize)]
struct AgentRow {
    label: String,
    role: String,
    /// `—` for master; `commit` / `gate` for reviewers.
    review: String,
    bound: bool,
    tool: Option<String>,
    session_id: Option<String>,
}

impl AgentRow {
    fn from_join(
        label: &AgentLabel,
        role: &str,
        review: &str,
        declared_tool: Tool,
        session: Option<&Session>,
    ) -> Self {
        let (bound, tool_str, session_id) = match session {
            // Session.tool wins for the displayed tool when bound —
            // it's the tool actually running the agent.
            Some(s) => (
                true,
                Some(s.tool.as_str().to_string()),
                Some(s.id.as_str().to_string()),
            ),
            None => (false, Some(declared_tool.as_str().to_string()), None),
        };
        Self {
            label: label.as_str().to_string(),
            role: role.to_string(),
            review: review.to_string(),
            bound,
            tool: tool_str,
            session_id,
        }
    }
}

fn list(args: AgentListArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    // Registration is the resolved roster set; skeleton supplies
    // session state only.
    let Some(set) = crate::agent_store::try_resolve_via_team(&repo)? else {
        anyhow::bail!(
            "this repo has no master agent. Run `clank init --team <name>` or \
             `clank agent promote <agent>` to set one."
        );
    };
    let mut rows: Vec<AgentRow> = Vec::new();
    // Master first.
    {
        let skeleton = load_agent_config(&repo, &set.master)?;
        rows.push(AgentRow::from_join(
            &set.master,
            "master",
            "—",
            set.master_desc.tool,
            skeleton.as_ref().and_then(|c| c.session.as_ref()),
        ));
    }
    for r in &set.reviewers {
        let skeleton = load_agent_config(&repo, &r.label)?;
        rows.push(AgentRow::from_join(
            &r.label,
            "reviewer",
            role_word(r.role),
            r.desc.tool,
            skeleton.as_ref().and_then(|c| c.session.as_ref()),
        ));
    }
    if args.json {
        let s = serde_json::to_string_pretty(&rows)?;
        println!("{s}");
    } else {
        print_human(&rows);
    }
    Ok(())
}

fn print_human(rows: &[AgentRow]) {
    if rows.is_empty() {
        println!("no agents registered");
        return;
    }
    // Column widths sized to the data.
    let w_label = rows.iter().map(|r| r.label.len()).max().unwrap_or(5).max(5);
    let w_role = rows.iter().map(|r| r.role.len()).max().unwrap_or(4).max(4);
    let w_bound = 5; // "BOUND"
    let w_tool = rows
        .iter()
        .map(|r| r.tool.as_deref().unwrap_or("—").len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<w_label$}  {:<w_role$}  {:<w_bound$}  {:<w_tool$}  SESSION",
        "LABEL", "ROLE", "BOUND", "TOOL"
    );
    for r in rows {
        let bound = if r.bound { "yes" } else { "NO" };
        let tool = r.tool.as_deref().unwrap_or("—");
        let session = r.session_id.as_deref().unwrap_or("—");
        // Shorten session ID for the human view; JSON keeps the full id.
        let session_short: String = session.chars().take(8).collect();
        let session_display = if session == "—" {
            "—".to_string()
        } else {
            format!("{session_short}…")
        };
        println!(
            "{:<w_label$}  {:<w_role$}  {:<w_bound$}  {:<w_tool$}  {session_display}",
            r.label, r.role, bound, tool
        );
    }
}

/// `clank agent start <name>`: exec into the agent's CLI tool.
///
/// Two paths:
/// - **Resume** (declaration + bound session): exec with the
///   configured launch profile + session-restore suffix.
/// - **Bootstrap** (declaration but no bound session — skeleton
///   missing OR `session: None`): exec the bare tool with a seed
///   prompt instructing the agent to run `clank as <label>`. After
///   that bind, subsequent calls hit the resume path. See
///   `compose_bootstrap_launch` for the program-resolution policy.
///   Plan: `agent-start-bootstraps-missing-skeleton`.
///
/// Launch profile source: the merged declaration's `launch`
/// field (per `agent-add-cli-and-repo-scope`). Session binding
/// state comes from the per-agent skeleton.
fn start(args: AgentStartArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = AgentLabel::parse(&args.name)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.name))?;

    // Registration is the resolved roster set. The agent's
    // description (tool / launch / initial_prompt) comes from
    // there; the skeleton supplies only session + auto_mode.
    let Some(set) = crate::agent_store::try_resolve_via_team(&repo)? else {
        anyhow::bail!(
            "this repo has no master agent. Run `clank init --team <name>` or \
             `clank agent promote <agent>` to set one."
        );
    };
    let desc = find_in_set(&set, &label).ok_or_else(|| {
        anyhow::anyhow!(
            "no agent `{}` on this repo's roster (master/reviewers: {})",
            args.name,
            registered_labels(&set).join(", ")
        )
    })?;

    let cfg = load_agent_config(&repo, &label)?;
    let bound_session = cfg.as_ref().and_then(|c| c.session.as_ref());

    // Each arm captures the tool ITS composer runs — the fork spec's or
    // the bound session's tool wins over the roster description, which
    // can be stale (a codex session under a since-changed claude roster
    // entry must still get codex launch side effects; codex ff3ca44).
    let (composed, launch_tool) = match bound_session {
        // A seeded fork spec (clank fork) takes precedence over the
        // plain bootstrap on first launch: fork the source session
        // with the orientation prompt; the forked id then binds via
        // the env-var hook and later starts hit the resume path. The
        // spec is ONE-SHOT — consume (delete) it on a real launch so
        // a later relaunch with a lost binding can't re-fork the
        // ancestor (fork-session-id-chaining). `--print` only peeks.
        None => {
            let spec = if args.print {
                crate::cli::fork::load_fork_spec(&repo, &label)?
            } else {
                crate::cli::fork::take_fork_spec(&repo, &label)?
            };
            match spec {
                Some(spec) => {
                    let tool = spec.tool;
                    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                    (
                        compose_fork_launch(&label, &spec, &desc, &repo, home.as_deref()),
                        tool,
                    )
                }
                None => (compose_bootstrap_launch(&repo, &label, &desc)?, desc.tool),
            }
        }
        Some(session) => match session_holder::find(
            session,
            &label,
            &repo_as_given(&args, &repo),
            launch_program(Tool::Claude, desc.launch.as_ref()),
        ) {
            Holder::Pane {
                session: held_in,
                tab,
            } => anyhow::bail!(
                "`{label}` is already running in zellij session `{held_in}`, tab `{tab}`. \
                 This pane would resume the same {} conversation a second time, which {} \
                 refuses. Close that pane, or attach to that session instead \
                 (`zellij attach {held_in}`).",
                session.tool.as_str(),
                session.tool.as_str(),
            ),
            Holder::ClaudeBackground { short_id } => (
                compose_attach_launch(session.tool, desc.launch.as_ref(), &short_id),
                session.tool,
            ),
            Holder::Nobody => {
                let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                let auto_mode =
                    crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref());
                let resolved_prompt =
                    resolve_initial_prompt(desc.initial_prompt.as_deref(), auto_mode, session.tool);
                (
                    compose_launch(
                        &repo,
                        &label,
                        session,
                        desc.launch.as_ref(),
                        resolved_prompt.as_deref(),
                    ),
                    session.tool,
                )
            }
        },
    };

    if args.print {
        print_composed(&composed);
        return Ok(());
    }

    // Codex shows an interactive "do you trust this directory?" prompt
    // for any repo root not recorded in `~/.codex/config.toml` — and
    // every codex launch in the codebase flows through THIS exec
    // (every zellij pane runs `clank agent start`),
    // so ensuring trust here ensures it everywhere
    // (codex-trust-at-launch). Best-effort: a failure degrades to the
    // prompt, never a failed launch. Sits AFTER the `--print` return —
    // previews must not write.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    pre_launch_codex_trust(launch_tool, &repo, home.as_deref());

    exec_composed(composed)
}

/// The `--repo` this process was GIVEN, not the resolved path: the
/// pane's `terminal_command` carries the literal argument, and the
/// holder scan is byte matching against it.
fn repo_as_given(args: &AgentStartArgs, resolved: &Path) -> String {
    args.repo
        .as_deref()
        .unwrap_or(resolved)
        .to_string_lossy()
        .into_owned()
}

fn launch_program(tool: Tool, launch: Option<&LaunchConfig>) -> String {
    launch
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| tool.as_str().to_string())
}

/// `claude attach <short id>` and nothing else: `attach` is a
/// subcommand, and the profile's args belong to the process that is
/// already running with them. The env goes along as for any launch.
fn compose_attach_launch(
    tool: Tool,
    launch: Option<&LaunchConfig>,
    short_id: &str,
) -> ComposedLaunch {
    let mut env_overrides = tool_env_defaults(tool);
    env_overrides.extend(launch.map(|l| l.env.clone()).unwrap_or_default());
    ComposedLaunch {
        program: launch_program(tool, launch),
        args: vec!["attach".into(), short_id.into()],
        env_overrides,
    }
}

/// Grok's folder-trust prompt is bypassed per-launch: `--trust` ("trust
/// this folder and persist the decision", hidden from --help but real
/// in 0.2.93) persists the cwd into grok's own trust store — no config
/// surgery, and the composed argv shows it honestly in `--print`
/// (grok-first-class). No-op for other tools.
fn push_grok_trust(tool: Tool, args: &mut Vec<String>) {
    if tool == Tool::Grok {
        args.push("--trust".into());
    }
}

/// The pre-exec trust step: codex launches record the MAIN repo root
/// (codex's prompt applies trust to the repository root, so it covers
/// every worktree under it) as trusted; claude has no such prompt and
/// writes nothing.
fn pre_launch_codex_trust(tool: Tool, repo: &Path, home: Option<&Path>) {
    if tool != Tool::Codex {
        return;
    }
    let Some(home) = home else {
        return;
    };
    let root = match crate::cli::fork::main_repo_root(repo) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: could not resolve the main repo root for codex trust: {e}");
            return;
        }
    };
    if let Err(e) = ensure_codex_project_trust(home, &root) {
        eprintln!(
            "warning: could not record codex trust for {}: {e}",
            root.display()
        );
    }
}

/// Record `root` as a trusted codex project in `~/.codex/config.toml`
/// — exactly the two-line table codex writes when the user answers Yes
/// to its trust prompt. NO-OP when the root already has ANY
/// `[projects."<root>"]` entry: an existing entry is a prior user
/// decision (possibly an explicit distrust) and is never overridden.
/// Appends at the end of the file (TOML table headers are
/// position-independent), preserving the existing content verbatim;
/// creates the file when absent (codex-trust-at-launch).
fn ensure_codex_project_trust(home: &Path, root: &Path) -> anyhow::Result<()> {
    let path = home.join(".codex/config.toml");
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading `{}`", path.display())));
        }
    };
    let header = format!("[projects.\"{}\"]", root.display());
    if existing.contains(&header) {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("\n{header}\ntrust_level = \"trusted\"\n"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, out).with_context(|| format!("writing `{}`", path.display()))?;
    Ok(())
}

/// Find an agent's resolved `AgentDescription` in the registered
/// set, whether it's the master or a reviewer in either tier.
fn find_in_set(
    set: &crate::cli::teams_config::RegisteredSet,
    label: &AgentLabel,
) -> Option<AgentDescription> {
    if &set.master == label {
        return Some(set.master_desc.clone());
    }
    set.reviewers
        .iter()
        .find(|r| &r.label == label)
        .map(|r| r.desc.clone())
}

fn registered_labels(set: &crate::cli::teams_config::RegisteredSet) -> Vec<String> {
    let mut out = vec![set.master.as_str().to_string()];
    out.extend(set.reviewers.iter().map(|r| r.label.as_str().to_string()));
    out
}

/// Bootstrap launch: a registered agent has no bound session in
/// this repo. Spawn the tool (from its `AgentDescription`) with a
/// seed prompt instructing the agent to run `clank as <label>`,
/// which creates the skeleton + session binding. Subsequent
/// `clank agent start <label>` calls resume normally.
fn compose_bootstrap_launch(
    repo: &Path,
    label: &AgentLabel,
    desc: &AgentDescription,
) -> anyhow::Result<ComposedLaunch> {
    let program = desc
        .launch
        .as_ref()
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| desc.tool.as_str().to_string());

    let mut args = desc
        .launch
        .as_ref()
        .map(|l| l.args.clone())
        .unwrap_or_default();
    push_grok_trust(desc.tool, &mut args);
    let name = session_display_name(repo, label);
    set_session_name(desc.tool, &mut args, &name);
    push_prompt(
        desc.tool,
        &mut args,
        name_led_prompt(desc.tool, &name, bootstrap_bind_prompt(label)),
    );

    let mut env_overrides = tool_env_defaults(desc.tool);
    env_overrides.extend(
        desc.launch
            .as_ref()
            .map(|l| l.env.clone())
            .unwrap_or_default(),
    );

    Ok(ComposedLaunch {
        program,
        args,
        env_overrides,
    })
}

/// Launch a FORKED copy of a source session (clank fork): every
/// tool forks cleanly — `claude --resume <src> --fork-session`
/// mints a diverged session; `codex fork -C <worktree> <src>
/// [prompt]` likewise; `opencode --session <src> --fork` branches
/// under a fresh `ses_` id (verified live, opencode-agent-tool M2).
/// `worktree` is this agent's resolved repo (the fork dest).
/// codex's `-C/--cd` is REQUIRED: forking a session whose recorded
/// cwd differs from the launch cwd otherwise makes codex
/// interactively prompt "Choose working directory…" every time
/// (fork-codex-cd-flag); naming the worktree explicitly skips that
/// picker. claude takes the cwd from the pane and doesn't prompt.
/// The orientation prompt rides in each tool's prompt slot
/// ([`push_prompt`]).
fn compose_fork_launch(
    label: &AgentLabel,
    spec: &crate::cli::fork::ForkSpec,
    desc: &AgentDescription,
    worktree: &Path,
    home: Option<&Path>,
) -> ComposedLaunch {
    let program = desc
        .launch
        .as_ref()
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| spec.tool.as_str().to_string());
    let mut args = desc
        .launch
        .as_ref()
        .map(|l| l.args.clone())
        .unwrap_or_default();
    // Boundary TWO. The seed was validated when the fork was built,
    // but a transcript can be deleted in between, and `--resume` on a
    // dead one exits immediately with no pane and no error. Re-probe
    // here so the guarantee holds at the moment it matters. `None`
    // (unprobeable) still resumes — it is not evidence of absence.
    let live = match &spec.from_session {
        Some(sid) => {
            crate::session_probe::session_jsonl_exists(&spec.tool, sid, home) != Some(false)
        }
        None => true,
    };
    let from_session = if live {
        spec.from_session.clone()
    } else {
        None
    };
    if !live {
        eprintln!(
            "warning: forked session is gone — starting a fresh session with the \
             fork's orientation prompt"
        );
    }
    match (&from_session, spec.tool) {
        (Some(sid), Tool::Claude) => {
            args.push("--resume".into());
            args.push(sid.clone());
            args.push("--fork-session".into());
        }
        (Some(sid), Tool::Codex) => {
            args.push("fork".into());
            args.push("-C".into());
            args.push(worktree.display().to_string());
            args.push(sid.clone());
        }
        // Grok forks like claude: resume the source session and branch
        // it under a fresh id grok generates (grok-first-class). The
        // new id binds later via `clank as` (newest-session-for-cwd).
        (Some(sid), Tool::Grok) => {
            args.push("--resume".into());
            args.push(sid.clone());
            args.push("--fork-session".into());
        }
        // opencode: `--session <src> --fork` — the TUI resumes a COPY
        // and mints a fresh `ses_` id; the source is untouched. The
        // new id binds later via the plugin + `clank as` (same as a
        // fresh start). cwd comes from the pane, like claude.
        (Some(sid), Tool::OpenCode) => {
            args.push("--session".into());
            args.push(sid.clone());
            args.push("--fork".into());
        }
        // No source session to fork (fork-robustness): launch a FRESH
        // session — same shape as the bootstrap launch, but carrying
        // the fork's orientation prompt instead of the bare bind hint.
        (None, _) => {}
    }
    push_grok_trust(spec.tool, &mut args);
    // Named for the FORK's worktree, not the source's. A forked
    // session otherwise keeps whatever name it was copied from, which
    // is the one name guaranteed to be wrong — and for the derived
    // tools the orientation prompt opens with "You are `x` in clone
    // …", putting the fork name mid-string, exactly where a truncated
    // title cuts it off.
    let name = session_display_name(worktree, label);
    set_session_name(spec.tool, &mut args, &name);
    push_prompt(
        spec.tool,
        &mut args,
        name_led_prompt(spec.tool, &name, spec.prompt.clone()),
    );
    let mut env_overrides = tool_env_defaults(spec.tool);
    env_overrides.extend(
        desc.launch
            .as_ref()
            .map(|l| l.env.clone())
            .unwrap_or_default(),
    );
    ComposedLaunch {
        program,
        args,
        env_overrides,
    }
}

/// Append the launch prompt in the tool's own dialect: opencode's
/// TUI positional is a PROJECT PATH (a prompt there would be read as
/// a directory), so its prompt rides behind `--prompt`; every other
/// tool takes the trailing positional.
fn push_prompt(tool: Tool, args: &mut Vec<String>, prompt: String) {
    if tool == Tool::OpenCode {
        args.push("--prompt".into());
    }
    args.push(prompt);
}

/// The display name a session should carry: the worktree (or repo)
/// directory it belongs to, then the agent label.
///
/// The directory alone was the literal ask, but several agents share
/// one worktree, so it collides across the master and every reviewer
/// there — and an undistinguishable session list is the complaint
/// being fixed. Leading with the place keeps the ask; the label makes
/// it answer the question.
pub(super) fn session_display_name(repo: &Path, label: &AgentLabel) -> String {
    let place = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.to_string_lossy().into_owned());
    format!("{place} · {}", label.as_str())
}

/// Set the session name where the tool accepts one outright.
///
/// Only `claude` does (`-n, --name`, top level, so it applies to the
/// interactive launch). Passed on EVERY launch including resume: with
/// no way to read a session's current name back, "set it once" would
/// leave every session created before this permanently mis-titled,
/// which is the reported problem.
fn set_session_name(tool: Tool, args: &mut Vec<String>, name: &str) {
    if tool == Tool::Claude {
        args.push("-n".into());
        args.push(name.to_string());
    }
}

/// Lead the prompt with the name for tools that DERIVE a title from
/// the opening message.
///
/// `opencode` has `--title` only on its non-interactive `run`, and
/// `codex` has no launch-time flag at all — both summarise the first
/// message instead, which opencode's own flag doc states: "uses
/// truncated prompt if no value provided". Since clank writes that
/// message, the name is still clank-driven; nothing here asks the
/// agent to name itself.
///
/// The tool decides the final string, so this SHAPES a derived title
/// rather than setting one.
fn name_led_prompt(tool: Tool, name: &str, prompt: String) -> String {
    if tool == Tool::Claude {
        prompt
    } else {
        format!("{name} — {prompt}")
    }
}

/// Seed prompt for the bootstrap launch. Verbatim per the plan's
/// PINNED string — `bootstrap_uses_tool_from_declaration` test
/// asserts on this with `assert_eq!`, so a wording tweak fails the
/// test deliberately.
pub(super) fn bootstrap_bind_prompt(label: &AgentLabel) -> String {
    format!("Run `clank as {}` to bind this session.", label.as_str())
}

/// Env defaults a tool's launch always carries; `launch.env` merges
/// OVER these, so a user can override. opencode: its claude-compat
/// scan surfaces ~/.claude/skills AND wins the name dedupe, so the
/// claude-flavored role skills (which teach background-wait arming —
/// the wrong loop for a plugin-driven tool) would shadow the
/// opencode-composed copies setup installs to
/// ~/.config/opencode/skills. Disabling the scan gives opencode the
/// same per-tool skill model codex and grok already have.
fn tool_env_defaults(tool: Tool) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    if tool == Tool::OpenCode {
        env.insert("OPENCODE_DISABLE_CLAUDE_CODE_SKILLS".into(), "1".into());
    }
    env
}

/// Composed launch line: executable + argv + env additions.
#[derive(Debug, Clone)]
struct ComposedLaunch {
    program: String,
    args: Vec<String>,
    env_overrides: std::collections::BTreeMap<String, String>,
}

/// Default `initial_prompt` for CLAUDE/CODEX/OPENCODE when
/// `auto_mode == On` and the declaration's `initial_prompt` field is
/// unset. Verbatim per `agent-start-initial-prompt` Phase 3:
/// - Triggers turn-end with minimum surface (a one-line ack).
/// - Does NOT instruct the agent to run `clank wait` itself (an
///   orchestrator reacts to the turn ending: claude/codex via their
///   stop hook, opencode via the clank plugin's `session.idle`
///   handler — opencode-agent-tool M1; a wait instruction here
///   would double-trigger). Grok has no orchestrator and gets
///   [`DEFAULT_AUTO_PROMPT_GROK`] instead.
/// - Does NOT expose orchestration internals (no "stop hook",
///   no "work loop") — agent only learns contextual location.
///
/// Test `resolve_initial_prompt_uses_default_when_auto_on_and_declaration_unset`
/// asserts on this string with `assert_eq!`, so a tweak fails
/// the test deliberately.
pub(super) const DEFAULT_AUTO_PROMPT: &str = "Session resumed.";

/// Grok's default when `auto_mode == On`: unlike claude/codex, grok
/// has NO clank stop hook (its hooks are passive — grok-first-class),
/// so nothing orchestrates it after the ack turn. Without this
/// instruction a freshly started grok ends its first turn with no
/// wait armed and never wakes (grok-first-turn-orchestration). The
/// arming instruction IS the orchestrator for grok.
pub(super) const DEFAULT_AUTO_PROMPT_GROK: &str = "Session resumed. Arm your clank work loop \
     now: run `clank wait` as a background terminal command (background: true), then end your \
     turn. Its completion wakes you with work items.";

/// Resolve the initial prompt for an agent-start invocation.
///
/// Three-input policy:
/// - `declaration`: `AgentDescription.initial_prompt` from the
///   resolved registered set. `Some("")` means "explicitly
///   disable" — falls back to None, NOT Some("") through. Lets a
///   user with auto_mode=On opt out of the prompt without
///   disabling auto_mode itself.
/// - `auto_mode`: per-machine preference from the agent skeleton.
///   When On, supplies the [`DEFAULT_AUTO_PROMPT`] fallback.
/// - Returns the resolved prompt or None.
pub(super) fn resolve_initial_prompt(
    declaration: Option<&str>,
    auto_mode: AutoMode,
    tool: Tool,
) -> Option<String> {
    if let Some(s) = declaration {
        if s.is_empty() {
            return None;
        }
        return Some(s.to_string());
    }
    if auto_mode == AutoMode::On {
        // Tool-aware: for claude/codex the STOP HOOK orchestrates
        // after the ack (a `clank wait` instruction here would
        // double-trigger); grok has no hook, so its prompt must carry
        // the arming itself (grok-first-turn-orchestration).
        return Some(match tool {
            Tool::Grok => DEFAULT_AUTO_PROMPT_GROK.to_string(),
            // opencode joined the hook-driven side once the M1 spike
            // proved the plugin loop (session.idle → stop-hook →
            // inject) — the grok-style arming interim is gone.
            Tool::Claude | Tool::Codex | Tool::OpenCode => DEFAULT_AUTO_PROMPT.to_string(),
        });
    }
    None
}

fn compose_launch(
    repo: &Path,
    label: &AgentLabel,
    session: &Session,
    launch: Option<&LaunchConfig>,
    initial_prompt: Option<&str>,
) -> ComposedLaunch {
    let tool = session.tool;
    let program = launch_program(tool, launch);

    let launch_args = launch.map(|l| l.args.clone()).unwrap_or_default();

    let session_restore = session_restore_args(tool, session.id.as_str(), repo);

    let mut args = launch_args;
    args.extend(session_restore);
    push_grok_trust(tool, &mut args);
    let name = session_display_name(repo, label);
    set_session_name(tool, &mut args, &name);
    if let Some(prompt) = initial_prompt {
        push_prompt(
            tool,
            &mut args,
            name_led_prompt(tool, &name, prompt.to_string()),
        );
    }

    let mut env_overrides = tool_env_defaults(tool);
    env_overrides.extend(launch.map(|l| l.env.clone()).unwrap_or_default());

    ComposedLaunch {
        program,
        args,
        env_overrides,
    }
}

/// Tool-specific session-restore suffix. Goes AFTER `launch.args`
/// so codex's `resume` subcommand doesn't capture them (see Phase
/// B step 3 of the plan). The initial prompt (when present) is
/// appended by [`compose_launch`] AFTER this suffix — for codex
/// that means the prompt is the final positional, AFTER `--cd
/// <repo>` (clap parses options + positionals independently, so
/// the option/positional ordering is flexible).
fn session_restore_args(tool: Tool, session_id: &str, repo: &Path) -> Vec<String> {
    match tool {
        Tool::Claude => vec!["--resume".into(), session_id.into()],
        Tool::Codex => vec![
            "resume".into(),
            session_id.into(),
            "--cd".into(),
            repo.to_string_lossy().into_owned(),
        ],
        // Grok resumes by id; --cwd pins the working directory (its
        // session store groups by cwd, but --resume finds ids anywhere).
        Tool::Grok => vec![
            "--resume".into(),
            session_id.into(),
            "--cwd".into(),
            repo.to_string_lossy().into_owned(),
        ],
        // Per `opencode run --help`: `-s/--session <id>` continues a
        // session; the M1 spike verifies the TUI accepts the same
        // (opencode-agent-tool M0 — full start/resume is M2).
        Tool::OpenCode => vec!["--session".into(), session_id.into()],
    }
}

/// Print the composed launch: shell-quoted argv on stdout, env
/// diff on stderr. Used by `--print` for tests + zellij-style
/// introspection.
fn print_composed(c: &ComposedLaunch) {
    let mut line = shell_quote(&c.program);
    for a in &c.args {
        line.push(' ');
        line.push_str(&shell_quote(a));
    }
    println!("{line}");
    if !c.env_overrides.is_empty() {
        for (k, v) in &c.env_overrides {
            eprintln!("env: {k}={v}");
        }
    }
}

/// A fresh agent launch must not INHERIT another agent's identity:
/// agent sessions export their session vars to every shell, so a
/// tool started from inside one (or from a pane server that kept
/// them) sees the parent's identity and either dies in
/// ConflictingSessions or silently binds as the parent — observed
/// live with opencode under a claude shell (opencode-agent-tool M1).
/// Each tool re-exports its own identity to its children, so
/// scrubbing is always safe; deliberate `launch.env` overrides are
/// applied after and win.
fn scrub_inherited_identity(cmd: &mut std::process::Command) {
    for var in crate::agent_env::SESSION_IDENTITY_VARS {
        cmd.env_remove(var);
    }
}

#[cfg(unix)]
fn exec_composed(c: ComposedLaunch) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&c.program);
    cmd.args(&c.args);
    scrub_inherited_identity(&mut cmd);
    for (k, v) in &c.env_overrides {
        cmd.env(k, v);
    }
    // `exec` replaces the current process; the only return
    // value is the io::Error if exec itself fails (e.g.
    // command-not-found, permissions).
    let err = cmd.exec();
    Err(anyhow::anyhow!("failed to exec `{}`: {err}", c.program))
}

#[cfg(not(unix))]
fn exec_composed(c: ComposedLaunch) -> anyhow::Result<()> {
    // Non-unix fallback: spawn + wait. Process replacement isn't
    // available on Windows; the calling shell sees clank exit
    // with the child's status.
    let mut cmd = std::process::Command::new(&c.program);
    cmd.args(&c.args);
    scrub_inherited_identity(&mut cmd);
    let status = cmd
        .envs(c.env_overrides.iter())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn `{}`: {e}", c.program))?;
    if !status.success() {
        anyhow::bail!("`{}` exited with status {}", c.program, status);
    }
    Ok(())
}

// ── clank agent add / promote / remove ────────────────────
//
// The repo's `agents` IS the roster. `agent add` is ONE step
// (definition + role). Three modes:
//   - `--global --tool`: write a role-free DESCRIPTION into the
//     user-scope `agents` LIBRARY (no repo change);
//   - repo `--tool`: define a fresh agent inline → insert a
//     RosterAgent with the `--review` role;
//   - repo by-name (no `--tool`): copy a description from the
//     user-scope library → insert a RosterAgent with the role.
// Per-agent skeletons hold only state (session / auto_mode /
// wake sources) — never declaration.

/// `clank agent add <label> [--global] [--tool ...] [--review ...]`
/// — thin shell: parse the label + role, build the launch profile,
/// dispatch to the matching `pub` core by scope/tool.
fn add(args: AgentAddArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let role: RosterRole = ReviewKind::from(args.review).into();

    if args.global {
        let tool = args.tool.ok_or_else(|| {
            anyhow::anyhow!("--global requires --tool (a library description needs a tool)")
        })?;
        let env = parse_env_overrides(&args.launch_envs)?;
        let launch = build_launch(args.launch_cmd, args.launch_args, env);
        let desc = AgentDescription {
            tool: tool.into(),
            launch,
            initial_prompt: args.initial_prompt.clone(),
        };
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        declare_global_agent(home_ref, &label, desc)?;
        return Ok(());
    }

    let repo = resolve_repo(args.repo.as_deref())?;
    match args.tool {
        // Inline definition: a fresh agent built from CLI flags.
        Some(tool) => {
            let env = parse_env_overrides(&args.launch_envs)?;
            let launch = build_launch(args.launch_cmd, args.launch_args, env);
            let desc = AgentDescription {
                tool: tool.into(),
                launch,
                initial_prompt: args.initial_prompt.clone(),
            };
            add_repo_roster_agent(&repo, &label, desc, role)?;
        }
        // By-name: copy the description from the user-scope library.
        None => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            add_repo_roster_agent_by_name(&repo, home.as_deref(), &label, role)?;
        }
    }
    // No zellij projection here: the status TUI owns roster→pane
    // convergence (tui-zellij-pane-reconcile), so this command — like
    // every other roster write path — is a pure config mutation.
    Ok(())
}

// ── agent mutation cores ─────────────────────────────────────
//
// `pub`, env-free (explicit `home`/`repo`), args-free. Both
// `run()` and integration-test setup call these.

/// How a roster transition treats labels newly entering reviewer tiers.
/// Every repo-roster write must choose here; identity replacement is the
/// deliberate exception to historical gate preservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RosterTransitionPolicy {
    PreserveGate,
    RequireFreshReview,
}

#[derive(Debug, Clone)]
struct GateTarget {
    sha: clank_core::ids::CommitSha,
    /// A shared commit can be latest for several plans with different
    /// milestone facts. One stand-in must preserve every context.
    touched_plan: Vec<bool>,
    reviews: Vec<clank_core::wait::ReviewEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RosterStandIn {
    author: AgentLabel,
    sha: clank_core::ids::CommitSha,
    verdict: clank_core::Verdict,
}

fn added_expected_labels(
    before: &crate::agent_store::ReviewerTiers,
    after: &crate::agent_store::ReviewerTiers,
) -> BTreeSet<AgentLabel> {
    let mut added = BTreeSet::new();
    for (old, new) in [
        (&before.commit, &after.commit),
        (&before.plan, &after.plan),
        (&before.final_, &after.final_),
    ] {
        added.extend(new.iter().filter(|label| !old.contains(label)).cloned());
    }
    added
}

fn remove_labels(tiers: &mut crate::agent_store::ReviewerTiers, labels: &BTreeSet<AgentLabel>) {
    tiers.commit.retain(|label| !labels.contains(label));
    tiers.plan.retain(|label| !labels.contains(label));
    tiers.final_.retain(|label| !labels.contains(label));
}

fn add_label_from_after(
    tiers: &mut crate::agent_store::ReviewerTiers,
    after: &crate::agent_store::ReviewerTiers,
    label: &AgentLabel,
) {
    for (current, final_tier) in [
        (&mut tiers.commit, &after.commit),
        (&mut tiers.plan, &after.plan),
        (&mut tiers.final_, &after.final_),
    ] {
        if final_tier.contains(label) && !current.contains(label) {
            current.push(label.clone());
            current.sort();
        }
    }
}

fn gates_for_target(
    target: &GateTarget,
    tiers: &crate::agent_store::ReviewerTiers,
) -> Vec<clank_core::vocab::CommitGateState> {
    target
        .touched_plan
        .iter()
        .map(|touched_plan| {
            clank_core::wait::compute_gate(
                &target.reviews,
                &tiers.commit,
                &tiers.plan,
                &tiers.final_,
                *touched_plan,
            )
        })
        .collect()
}

/// Pure gate-preservation planner. Removals and tier exits are already present
/// in `after`; new expected labels are added one at a time so each pending slot
/// is neutralised only when it would otherwise alter the baseline gate.
fn plan_roster_stand_ins(
    before: crate::agent_store::ReviewerTiers,
    after: crate::agent_store::ReviewerTiers,
    policy: RosterTransitionPolicy,
    targets: &mut [GateTarget],
) -> anyhow::Result<Vec<RosterStandIn>> {
    if policy == RosterTransitionPolicy::RequireFreshReview {
        return Ok(Vec::new());
    }

    let entrants = added_expected_labels(&before, &after);
    let mut current = after.clone();
    remove_labels(&mut current, &entrants);
    let mut stand_ins = Vec::new();

    for entrant in entrants {
        let baseline: Vec<_> = targets
            .iter()
            .map(|target| gates_for_target(target, &current))
            .collect();
        add_label_from_after(&mut current, &after, &entrant);

        for (target, expected_gates) in targets.iter_mut().zip(baseline) {
            // A real (or earlier synthetic) verdict is historical evidence and
            // must never be replaced. Its own effect on the gate is legitimate.
            if target.reviews.iter().any(|review| review.author == entrant) {
                continue;
            }
            if gates_for_target(target, &current) == expected_gates {
                continue;
            }

            let verdict = [clank_core::Verdict::Continue, clank_core::Verdict::Finished]
                .into_iter()
                .find(|verdict| {
                    target.reviews.push(clank_core::wait::ReviewEntry {
                        author: entrant.clone(),
                        verdict: *verdict,
                    });
                    let preserves = gates_for_target(target, &current) == expected_gates;
                    target.reviews.pop();
                    preserves
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot preserve the gate for reviewer `{}` entering at {}",
                        entrant.as_str(),
                        target.sha.as_str()
                    )
                })?;

            target.reviews.push(clank_core::wait::ReviewEntry {
                author: entrant.clone(),
                verdict,
            });
            stand_ins.push(RosterStandIn {
                author: entrant.clone(),
                sha: target.sha.clone(),
                verdict,
            });
        }
    }
    Ok(stand_ins)
}

fn latest_gate_targets(repo: &Path, state: &crate::repo_state::RepoState) -> Vec<GateTarget> {
    use clank_core::wait::PlanStateLookup;

    let lookup = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let mut contexts: BTreeMap<clank_core::ids::CommitSha, Vec<bool>> = BTreeMap::new();
    for plan in state.fold.plans.values() {
        let Some(event) = plan
            .commits
            .iter()
            .rev()
            .find(|event| event.touched_plan || event.touched_code)
        else {
            continue;
        };
        let touched = contexts.entry(event.sha.clone()).or_default();
        if !touched.contains(&event.touched_plan) {
            touched.push(event.touched_plan);
        }
    }
    contexts
        .into_iter()
        .map(|(sha, touched_plan)| GateTarget {
            reviews: lookup.reviews_for(&sha),
            sha,
            touched_plan,
        })
        .collect()
}

fn all_reviewable_shas(state: &crate::repo_state::RepoState) -> Vec<clank_core::ids::CommitSha> {
    let mut shas = Vec::new();
    for plan in state.fold.plans.values() {
        shas.extend(plan.reviewable_shas());
    }
    shas.extend(state.fold.ad_hoc.iter().map(|event| event.sha.clone()));
    shas.sort();
    shas.dedup();
    shas
}

fn existing_feedback_path(
    repo: &Path,
    author: &AgentLabel,
    sha: &clank_core::ids::CommitSha,
) -> Option<PathBuf> {
    let dir = repo
        .join(".clank/agents")
        .join(author.as_str())
        .join("feedback");
    let full = dir.join(format!("{}.md", sha.as_str()));
    let short = dir.join(format!("{}.md", &sha.as_str()[..7.min(sha.as_str().len())]));
    [full, short].into_iter().find(|path| path.exists())
}

fn stand_in_body(verdict: clank_core::Verdict) -> String {
    let header = match verdict {
        clank_core::Verdict::Continue => "CONTINUE",
        clank_core::Verdict::Finished => "FINISHED",
        clank_core::Verdict::RequestChanges | clank_core::Verdict::Unmarked => {
            unreachable!("stand-ins are positive verdicts")
        }
    };
    format!(
        "{header} Synthetic roster stand-in; no review performed\n\n{}\n\
         This agent joined the review tier after the commit. Clank recorded\n\
         non-participation only to keep the already-reached gate from rewinding.\n\
         \n\
         The roster change produced this verdict, not a human judgment. If this\n\
         commit deserves a real review round, run `clank rereview` — it rewrites\n\
         the commit unchanged so every reviewer, this agent included, owes it a\n\
         fresh verdict.\n",
        clank_core::feedback_body::ROSTER_STAND_IN_MARKER
    )
}

fn write_stand_in_noclobber(
    repo: &Path,
    stand_in: &RosterStandIn,
    all_shas: &[clank_core::ids::CommitSha],
) -> anyhow::Result<Option<PathBuf>> {
    if existing_feedback_path(repo, &stand_in.author, &stand_in.sha).is_some() {
        return Ok(None);
    }
    let rel = crate::disk_format::feedback_path_wire(&stand_in.author, &stand_in.sha, all_shas);
    let path = repo.join(rel);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-roster-stand-in-")
        .suffix(".md.tmp")
        .tempfile_in(parent)?;
    use std::io::Write;
    tmp.write_all(stand_in_body(stand_in.verdict).as_bytes())?;
    tmp.as_file_mut().sync_all()?;
    match tmp.persist_noclobber(&path) {
        Ok(_) => Ok(Some(path)),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e.error.into()),
    }
}

fn write_repo_transition(
    repo: &Path,
    before: &RepoConfigFile,
    after: &RepoConfigFile,
    policy: RosterTransitionPolicy,
) -> anyhow::Result<()> {
    // No active plan means there is no plan gate to preserve and no reason to
    // pay for a fold. This also keeps pre-adoption roster setup independent of
    // git history.
    let plans_dir = repo.join(".clank/plans");
    let (stand_ins, all_shas) =
        if plans_dir.is_dir() && policy == RosterTransitionPolicy::PreserveGate {
            let state = crate::rebuild::rebuild_repo_sync_with_policy(
                repo,
                crate::rebuild::CachePolicy::Bypass,
            )?;
            let mut targets = latest_gate_targets(repo, &state);
            let before_tiers = crate::agent_store::ReviewerTiers::from_roster(&before.agents);
            let after_tiers = crate::agent_store::ReviewerTiers::from_roster(&after.agents);
            (
                plan_roster_stand_ins(before_tiers, after_tiers, policy, &mut targets)?,
                all_reviewable_shas(&state),
            )
        } else {
            (Vec::new(), Vec::new())
        };

    // Stand-ins are written first while their authors are still unexpected and
    // therefore gate-neutral. Noclobber protects a concurrent or historical
    // human review. Roll back only files created by this call if config
    // persistence fails.
    let mut created = Vec::new();
    for stand_in in &stand_ins {
        match write_stand_in_noclobber(repo, stand_in, &all_shas) {
            Ok(Some(path)) => {
                // Say it out loud. A synthetic verdict nobody is told
                // about is one nobody knows to question, and the whole
                // point of `rereview` is that it can be questioned.
                eprintln!(
                    "auto-continued {} for `{}` (roster change, not a review) — \
                     `clank rereview` opens a real round",
                    &stand_in.sha.as_str()[..7.min(stand_in.sha.as_str().len())],
                    stand_in.author.as_str()
                );
                created.push(path)
            }
            Ok(None) => {}
            Err(error) => {
                for path in created {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error);
            }
        }
    }
    if let Err(error) = write_repo_config_raw(repo, after) {
        for path in created {
            let _ = std::fs::remove_file(path);
        }
        return Err(error);
    }
    Ok(())
}

/// Declare an agent DESCRIPTION in the user-scope `agents`
/// library (role-free). Errors if the label already exists there.
pub fn declare_global_agent(
    home: &Path,
    label: &AgentLabel,
    desc: AgentDescription,
) -> anyhow::Result<()> {
    let mut file = read_user_config(home)?;
    if file.agents.contains_key(label) {
        anyhow::bail!(
            "agent `{}` already declared in user-scope `agents`",
            label.as_str()
        );
    }
    file.agents.insert(label.clone(), desc);
    write_user_config(home, &file)?;
    eprintln!("declared `{}` in user-scope `agents`", label.as_str());
    Ok(())
}

/// Insert a fresh agent (built inline from a description) into
/// THIS repo's roster with the given role. Errors if the label is
/// already on the roster (a roster entry is never silently
/// overwritten).
pub fn add_repo_roster_agent(
    repo: &Path,
    label: &AgentLabel,
    desc: AgentDescription,
    role: RosterRole,
) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;
    let before = repo_cfg.clone();
    if repo_cfg.agents.contains_key(label) {
        anyhow::bail!(
            "agent `{label}` is already on this repo's roster. \
             Remove it with `clank agent remove {label}` first, or pick a different name.",
            label = label.as_str()
        );
    }
    repo_cfg
        .agents
        .insert(label.clone(), RosterAgent::from_description(desc, role));
    write_repo_transition(
        repo,
        &before,
        &repo_cfg,
        RosterTransitionPolicy::PreserveGate,
    )?;
    eprintln!(
        "added `{}` to this repo's roster as a `{}` reviewer",
        label.as_str(),
        role_word(role)
    );
    Ok(())
}

/// Add `<label>` to THIS repo's roster BY NAME: copy its
/// description from the user-scope `agents` library and insert it
/// with the given role. Errors if the label isn't in the library
/// (suggesting `--tool` to define it inline, or `agent add
/// --global --tool` to populate the library first).
/// The description `<label>` carries in the user-scope `agents`
/// library. Errors with the two ways to define one when absent.
fn library_description(
    home: Option<&Path>,
    label: &AgentLabel,
) -> anyhow::Result<AgentDescription> {
    home.map(|h| -> anyhow::Result<Option<AgentDescription>> {
        Ok(read_user_config(h)?.agents.get(label).cloned())
    })
    .transpose()?
    .flatten()
    .ok_or_else(|| {
        anyhow::anyhow!(
            "unknown agent `{label}`: not in the user-scope `agents` library. \
             Define it inline with `clank agent add {label} --tool <claude|codex|grok|opencode>`, \
             or add it to the library first with \
             `clank agent add {label} --global --tool <claude|codex|grok|opencode>`.",
            label = label.as_str()
        )
    })
}

pub fn add_repo_roster_agent_by_name(
    repo: &Path,
    home: Option<&Path>,
    label: &AgentLabel,
    role: RosterRole,
) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;
    let before = repo_cfg.clone();
    if repo_cfg.agents.contains_key(label) {
        anyhow::bail!(
            "agent `{label}` is already on this repo's roster. \
             Remove it with `clank agent remove {label}` first, or pick a different name.",
            label = label.as_str()
        );
    }
    let desc = library_description(home, label)?;
    repo_cfg
        .agents
        .insert(label.clone(), RosterAgent::from_description(desc, role));
    write_repo_transition(
        repo,
        &before,
        &repo_cfg,
        RosterTransitionPolicy::PreserveGate,
    )?;
    eprintln!(
        "added `{}` to this repo's roster as a `{}` reviewer (copied from the library)",
        label.as_str(),
        role_word(role)
    );
    Ok(())
}

/// Replace `<out>` on THIS repo's roster with `<in>`, carrying the
/// outgoing agent's role across unchanged.
///
/// ONE config write: the role is never observed vacant, so no pass can
/// see a gate whose expected reviewer set has silently shrunk.
///
/// Pure config, like every sibling roster command — the status TUI
/// owns roster→pane convergence (tui-zellij-pane-reconcile). Giving
/// this command its own pane orchestration would make it a second
/// owner, and a pane it created before the write would be
/// roster-absent, which is exactly what the reconciler destroys.
pub fn swap_repo_agent(
    repo: &Path,
    home: Option<&Path>,
    out: &AgentLabel,
    into: &AgentLabel,
) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;
    let before = repo_cfg.clone();
    let role = match repo_cfg.agents.get(out) {
        Some(a) => a.role,
        None => anyhow::bail!(
            "agent `{}` is not on this repo's roster, so there is nothing to swap out",
            out.as_str()
        ),
    };
    // A map keyed by label: inserting onto an existing one COLLAPSES
    // two entries into one. `out` would go, `into`'s tier would be
    // overwritten with `out`'s, and the roster would shrink by one
    // silently — the shrinking expected-reviewer set this command
    // exists to avoid.
    if repo_cfg.agents.contains_key(into) {
        anyhow::bail!(
            "agent `{into}` is already on this repo's roster, so swapping onto it \
             would drop an entry rather than replace one. To change a tier use \
             `clank agent set-review {into} <tier>`; to drop an agent use \
             `clank agent remove {out}`.",
            into = into.as_str(),
            out = out.as_str()
        );
    }
    let desc = library_description(home, into)?;
    repo_cfg.agents.remove(out);
    repo_cfg
        .agents
        .insert(into.clone(), RosterAgent::from_description(desc, role));
    write_repo_transition(
        repo,
        &before,
        &repo_cfg,
        RosterTransitionPolicy::RequireFreshReview,
    )?;
    eprintln!(
        "swapped `{}` out for `{}` as `{}`",
        out.as_str(),
        into.as_str(),
        role_word(role)
    );
    Ok(())
}

/// `clank agent swap <out> <in>` — thin shell.
fn swap(args: AgentSwapArgs) -> anyhow::Result<()> {
    let out = AgentLabel::parse(&args.out)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.out))?;
    let into = AgentLabel::parse(&args.into)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.into))?;
    let repo = resolve_repo(args.repo.as_deref())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    swap_repo_agent(&repo, home.as_deref(), &out, &into)
}

/// `clank agent promote <name>` — thin shell.
fn promote(args: AgentPromoteArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.name)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.name))?;
    let repo = resolve_repo(args.repo.as_deref())?;
    set_repo_master(&repo, &label)?;
    // No zellij relocation here: the status TUI observes the master
    // swap in the config and re-layouts (tui-zellij-pane-reconcile).
    Ok(())
}

/// Set THIS repo's master to `<label>`:
/// `repo.agents[<label>].role = Master`, demoting the previous
/// master (if any, and different) to `Commit`. The agent must
/// already be on the roster.
pub fn set_repo_master(repo: &Path, label: &AgentLabel) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;
    let before = repo_cfg.clone();
    if !repo_cfg.agents.contains_key(label) {
        anyhow::bail!(
            "agent `{label}` is not on this repo's roster. \
             Add it first with `clank agent add {label} --tool <claude|codex|grok|opencode>` \
             (or by name: `clank agent add {label}`).",
            label = label.as_str()
        );
    }

    // Already the master → no-op.
    if repo_cfg.agents.get(label).map(|a| a.role) == Some(RosterRole::Master) {
        eprintln!("note: `{}` is already this repo's master", label.as_str());
        return Ok(());
    }

    // Demote the previous master to commit.
    for (other, agent) in repo_cfg.agents.iter_mut() {
        if other != label && agent.role == RosterRole::Master {
            agent.role = RosterRole::Commit;
        }
    }
    if let Some(agent) = repo_cfg.agents.get_mut(label) {
        agent.role = RosterRole::Master;
    }
    write_repo_transition(
        repo,
        &before,
        &repo_cfg,
        RosterTransitionPolicy::PreserveGate,
    )?;
    eprintln!("set `{}` as this repo's master", label.as_str());
    Ok(())
}

/// `clank agent set-review <name> <commit|gate>` — thin shell.
fn set_review(args: AgentSetReviewArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.name)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.name))?;
    let repo = resolve_repo(args.repo.as_deref())?;
    set_repo_review(&repo, &label, ReviewKind::from(args.review))
}

/// Change a reviewer's tier (`Commit` ↔ `Gate`) IN PLACE — a pure config
/// edit, so the agent's bound session and zellij pane are untouched (the
/// pane title is `"<name> (reviewer)"` for both tiers — nothing visual
/// changes). Takes [`ReviewKind`] (which has no `Master` variant) so the
/// "this only ever sets a reviewer tier" invariant is compiler-enforced —
/// a caller cannot ask to write a second master. Refuses if `<label>` is
/// the master (changing the master is `clank agent promote`, which
/// auto-demotes the old one) or isn't on the roster; no-op if already at
/// `tier`.
pub fn set_repo_review(repo: &Path, label: &AgentLabel, tier: ReviewKind) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;
    let before = repo_cfg.clone();
    let agent = match repo_cfg.agents.get_mut(label) {
        Some(a) => a,
        None => anyhow::bail!(
            "agent `{label}` is not on this repo's roster. \
             Add it with `clank agent add {label}` first.",
            label = label.as_str()
        ),
    };
    if agent.role == RosterRole::Master {
        anyhow::bail!(
            "agent `{}` is the master, not a reviewer. To change the master, \
             promote a different agent with `clank agent promote <other>` \
             (which demotes the current master).",
            label.as_str()
        );
    }
    let new_role = RosterRole::from(tier);
    if agent.role == new_role {
        eprintln!(
            "note: `{}` is already a `{}` reviewer",
            label.as_str(),
            role_word(new_role)
        );
        return Ok(());
    }
    agent.role = new_role;
    write_repo_transition(
        repo,
        &before,
        &repo_cfg,
        RosterTransitionPolicy::PreserveGate,
    )?;
    eprintln!(
        "set `{}` as a `{}` reviewer",
        label.as_str(),
        role_word(new_role)
    );
    Ok(())
}

/// `clank agent remove <label> [--global]` — thin shell.
fn remove(args: AgentRemoveArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;

    if args.global {
        // User-scope op: needs only $HOME, never repo discovery (mirror
        // `agent add --global`).
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        remove_global_agent(home_ref, &label)?;
    } else {
        let repo = resolve_repo(args.repo.as_deref())?;
        // Capture the role BEFORE removal: only a reviewer's pane is in
        // scope (master panes are deliberately left alone).
        remove_repo_agent(&repo, &label)?;
        // Pane closure moved to the TUI's roster→pane reconciler
        // (tui-zellij-pane-reconcile).
    }
    Ok(())
}

/// Remove an agent DESCRIPTION from the user-scope `agents`
/// library and scrub every team member that REFERENCES it (a
/// [`crate::cli::teams_config::TeamMember::Ref`] resolving to this
/// label) so no dangling reference survives. Inline team members are
/// self-contained and left untouched.
pub fn remove_global_agent(home: &Path, label: &AgentLabel) -> anyhow::Result<()> {
    use crate::cli::teams_config::TeamMember;
    let mut file = read_user_config(home)?;
    if file.agents.remove(label).is_none() {
        anyhow::bail!("agent `{}` not in user-scope `agents`", label.as_str());
    }
    let mut touched = Vec::new();
    for (name, team) in file.teams.iter_mut() {
        let before = team.len();
        team.retain(|key, member| match member {
            TeamMember::Ref(r) => r.agent.as_ref().unwrap_or(key) != label,
            TeamMember::Inline(_) => true,
        });
        if team.len() != before {
            touched.push(name.clone());
        }
    }
    write_user_config(home, &file)?;
    if touched.is_empty() {
        eprintln!("removed `{}` from user-scope `agents`", label.as_str());
    } else {
        eprintln!(
            "removed `{}` from user-scope `agents` (and from teams: {})",
            label.as_str(),
            touched.join(", ")
        );
    }
    Ok(())
}

/// Remove an agent from THIS repo's roster. Errors if the label
/// isn't on the roster.
pub fn remove_repo_agent(repo: &Path, label: &AgentLabel) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;
    let before = repo_cfg.clone();
    if repo_cfg.agents.remove(label).is_none() {
        anyhow::bail!("agent `{}` is not on this repo's roster", label.as_str());
    }
    write_repo_transition(
        repo,
        &before,
        &repo_cfg,
        RosterTransitionPolicy::PreserveGate,
    )?;
    eprintln!("removed `{}` from this repo's roster", label.as_str());
    Ok(())
}

/// Human word for a non-master roster role (used in `add`
/// confirmations).
fn role_word(role: RosterRole) -> &'static str {
    match role {
        RosterRole::Master => "master",
        RosterRole::Commit => "commit",
        RosterRole::Plan => "plan",
        RosterRole::Final => "final",
        RosterRole::Gate => "gate",
    }
}

fn parse_env_overrides(raw: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for s in raw {
        let (k, v) = s
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--launch-env value `{s}` must be `KEY=VAL`"))?;
        if k.is_empty() {
            anyhow::bail!("--launch-env key cannot be empty: `{s}`");
        }
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

/// Construct a `LaunchConfig` from CLI args. Returns `None` when
/// no launch-related flag was passed (Phase 6 default: skeleton
/// declaration's `launch` field is absent rather than `Some({})`).
fn build_launch(
    cmd: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
) -> Option<LaunchConfig> {
    if cmd.is_none() && args.is_empty() && env.is_empty() {
        return None;
    }
    Some(LaunchConfig {
        command: cmd,
        args,
        env,
    })
}

/// Read `~/.clank/config.json` as a typed [`UserConfigFile`].
/// Missing file → default; malformed JSON → error.
fn read_user_config(home: &Path) -> anyhow::Result<UserConfigFile> {
    let path = home.join(".clank/config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| anyhow::Error::from(e).context(format!("parsing {}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(UserConfigFile::default()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// Atomic write of `~/.clank/config.json`. Round-trip preserves
/// `review`, `hooks`, and any unknown fields (via the `extra`
/// flatten catchall on [`UserConfigFile`]).
fn write_user_config(home: &Path, file: &UserConfigFile) -> anyhow::Result<()> {
    write_typed_config(&home.join(".clank/config.json"), file)
}

/// Read `<repo>/.clank/config.json` as the typed
/// [`RepoConfigFile`]. Missing file → default; malformed JSON →
/// error.
fn read_repo_config(repo: &Path) -> anyhow::Result<RepoConfigFile> {
    let path = repo.join(".clank/config.json");
    match std::fs::read_to_string(&path) {
        // Fail-closed on an old-shape config BEFORE accepting the parse:
        // a legacy `{"team":"dev"}` / stray `promoted` would otherwise
        // deserialize (the key lands in `extra`) and a roster mutation
        // would rewrite a mixed old/new config. Use the same raw guard
        // as `agent_store::load_repo_config` (codex a52d486).
        Ok(s) if crate::agent_store::is_legacy_repo_shape(&s) => {
            Err(crate::agent_store::legacy_repo_schema_error(&path))
        }
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RepoConfigFile::default()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// Atomic write of `<repo>/.clank/config.json`. The `extra`
/// flatten catchall preserves unknown sections on round-trip.
fn write_repo_config_raw(repo: &Path, file: &RepoConfigFile) -> anyhow::Result<()> {
    write_typed_config(&repo.join(".clank/config.json"), file)
}

fn write_typed_config<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    fn lbl(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    /// Every roster command is a pure config write; the status TUI
    /// owns roster→pane convergence (tui-zellij-pane-reconcile).
    ///
    /// `agent swap` rests on this entirely. A roster command that
    /// created a pane would be a SECOND owner of pane lifecycle, and a
    /// pane created before its config write is roster-absent — which
    /// is exactly what the reconciler destroys, so the command would
    /// race the owner into losing the position it meant to preserve.
    #[test]
    fn roster_commands_never_touch_zellij() {
        // Split so this assertion is not itself a match.
        let calls = [concat!("open_", "zellij"), concat!("zellij", "_action")];
        for (n, line) in include_str!("agent.rs").lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for c in calls {
                assert!(!code.contains(c), "agent.rs:{} calls {c}: {line}", n + 1);
            }
        }
    }

    use super::*;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    #[test]
    fn opencode_restore_args_use_session_flag() {
        // opencode-agent-tool M0: per `opencode run --help`,
        // -s/--session continues a session (the M1 spike verifies the
        // TUI form; full composition is M2).
        let args = session_restore_args(
            clank_core::Tool::OpenCode,
            "ses_8f2a1b3c4d5e6f70",
            std::path::Path::new("/repo"),
        );
        assert_eq!(args, vec!["--session", "ses_8f2a1b3c4d5e6f70"]);
    }

    #[test]
    fn read_repo_config_fails_closed_on_legacy_shape() {
        // codex a52d486: a legacy `{"team":"dev"}` would deserialize (the
        // `team` key lands in `extra`) and a roster mutation would rewrite
        // a mixed old/new config. read_repo_config must fail closed with
        // the re-init hint instead, so agent add/remove/promote refuse.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(dir.path().join(".clank/config.json"), r#"{"team":"dev"}"#).unwrap();
        let err = read_repo_config(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("old team schema") && err.contains("clank init"),
            "expected re-init hint; got: {err}"
        );
    }

    fn make_session() -> Session {
        Session {
            id: clank_core::ids::SessionId::parse("11111111-1111-1111-1111-111111111111").unwrap(),
            tool: Tool::Claude,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }
    }

    // ── compose_launch initial-prompt tests ────────────────────

    fn claude_session() -> Session {
        Session {
            id: clank_core::ids::SessionId::parse("aaaaaaaa-1111-2222-3333-444444444444").unwrap(),
            tool: Tool::Claude,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }
    }

    fn codex_session() -> Session {
        Session {
            id: clank_core::ids::SessionId::parse("bbbbbbbb-1111-2222-3333-444444444444").unwrap(),
            tool: Tool::Codex,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }
    }

    fn grok_session() -> Session {
        Session {
            id: clank_core::ids::SessionId::parse("cccccccc-1111-2222-3333-444444444444").unwrap(),
            tool: Tool::Grok,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn an_attach_launch_is_the_subcommand_alone_under_the_profiles_program() {
        // The profile's args (`--agent <profile>`, …) belong to the
        // process already running with them; `attach` is a subcommand
        // and takes only the short id (a-session-has-one-holder).
        let launch = LaunchConfig {
            command: Some("/opt/bin/claude-wrapper".into()),
            args: vec!["--agent".into(), "reviewer".into()],
            env: BTreeMap::from([("CLANK_X".to_string(), "1".to_string())]),
        };
        let c = compose_attach_launch(Tool::Claude, Some(&launch), "1f47fd71");
        assert_eq!(c.program, "/opt/bin/claude-wrapper");
        assert_eq!(c.args, ["attach", "1f47fd71"]);
        assert_eq!(
            c.env_overrides.get("CLANK_X").map(String::as_str),
            Some("1")
        );
        let bare = compose_attach_launch(Tool::Claude, None, "1f47fd71");
        assert_eq!(bare.program, "claude");
        assert_eq!(bare.args, ["attach", "1f47fd71"]);
    }

    #[test]
    fn grok_launch_resumes_by_id_with_cwd_and_trust() {
        // grok-first-class: restore = --resume <id> --cwd <repo>, and
        // every grok launch carries --trust (folder-trust bypass) IN
        // the composed argv so --print previews it honestly.
        let s = grok_session();
        let c = compose_launch(Path::new("/repo"), &lbl("tester"), &s, None, Some("hello"));
        assert_eq!(c.program, "grok");
        let a: Vec<&str> = c.args.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            a,
            [
                "--resume",
                "cccccccc-1111-2222-3333-444444444444",
                "--cwd",
                "/repo",
                "--trust",
                // grok has no name flag either: derived from the prompt.
                "repo · tester — hello",
            ]
        );
    }

    #[test]
    fn claude_and_codex_launches_never_carry_trust_flag() {
        for s in [claude_session(), codex_session()] {
            let c = compose_launch(Path::new("/repo"), &lbl("tester"), &s, None, None);
            assert!(
                !c.args.iter().any(|a| a == "--trust"),
                "--trust is grok-only, got {:?} for {:?}",
                c.args,
                s.tool
            );
        }
    }

    fn seed_repo_config(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join(".clank")).unwrap();
        std::fs::write(repo.join(".clank/config.json"), body).unwrap();
    }

    fn read_repo(repo: &Path) -> crate::cli::teams_config::RepoConfigFile {
        let body = std::fs::read_to_string(repo.join(".clank/config.json")).unwrap();
        serde_json::from_str(&body).unwrap()
    }

    fn desc(tool: Tool) -> AgentDescription {
        AgentDescription {
            tool,
            launch: None,
            initial_prompt: None,
        }
    }

    fn tiers(commit: &[&str], plan: &[&str], final_: &[&str]) -> crate::agent_store::ReviewerTiers {
        crate::agent_store::ReviewerTiers {
            commit: commit.iter().map(|name| label(name)).collect(),
            plan: plan.iter().map(|name| label(name)).collect(),
            final_: final_.iter().map(|name| label(name)).collect(),
        }
    }

    fn gate_target(touched_plan: bool, reviews: &[(&str, clank_core::Verdict)]) -> GateTarget {
        GateTarget {
            sha: clank_core::ids::CommitSha::parse(&"a".repeat(40)).unwrap(),
            touched_plan: vec![touched_plan],
            reviews: reviews
                .iter()
                .map(|(author, verdict)| clank_core::wait::ReviewEntry {
                    author: label(author),
                    verdict: *verdict,
                })
                .collect(),
        }
    }

    fn target_gate(
        target: &GateTarget,
        tiers: &crate::agent_store::ReviewerTiers,
    ) -> clank_core::vocab::CommitGateState {
        gates_for_target(target, tiers)[0]
    }

    #[test]
    fn reviewer_add_preserves_every_computed_gate_state() {
        use clank_core::Verdict::{Continue, Finished, RequestChanges};
        use clank_core::vocab::CommitGateState;

        struct Case {
            name: &'static str,
            before: crate::agent_store::ReviewerTiers,
            after: crate::agent_store::ReviewerTiers,
            target: GateTarget,
            expected: CommitGateState,
            stand_in: Option<clank_core::Verdict>,
        }
        let cases = [
            Case {
                name: "unreviewed",
                before: tiers(&["alice"], &[], &[]),
                after: tiers(&["alice", "new"], &[], &[]),
                target: gate_target(false, &[]),
                expected: CommitGateState::Unreviewed,
                stand_in: None,
            },
            Case {
                name: "changes requested",
                before: tiers(&["alice"], &[], &[]),
                after: tiers(&["alice", "new"], &[], &[]),
                target: gate_target(false, &[("alice", RequestChanges)]),
                expected: CommitGateState::ChangesRequested,
                stand_in: Some(Continue),
            },
            Case {
                name: "continued",
                before: tiers(&["alice"], &[], &[]),
                after: tiers(&["alice", "new"], &[], &[]),
                target: gate_target(false, &[("alice", Continue)]),
                expected: CommitGateState::Continued,
                stand_in: Some(Continue),
            },
            Case {
                name: "continued pending gate",
                before: tiers(&["alice"], &["gate"], &[]),
                after: tiers(&["alice", "new"], &["gate"], &[]),
                target: gate_target(true, &[("alice", Continue)]),
                expected: CommitGateState::ContinuedPendingGate,
                stand_in: Some(Continue),
            },
            Case {
                name: "finished",
                before: tiers(&["alice"], &[], &["gate"]),
                after: tiers(&["alice"], &["new"], &["gate", "new"]),
                target: gate_target(false, &[("alice", Finished), ("gate", Finished)]),
                expected: CommitGateState::Finished,
                stand_in: Some(Finished),
            },
        ];

        for mut case in cases {
            assert_eq!(
                target_gate(&case.target, &case.before),
                case.expected,
                "{} baseline",
                case.name
            );
            let stand_ins = plan_roster_stand_ins(
                case.before,
                case.after.clone(),
                RosterTransitionPolicy::PreserveGate,
                std::slice::from_mut(&mut case.target),
            )
            .unwrap();
            assert_eq!(
                target_gate(&case.target, &case.after),
                case.expected,
                "{} after",
                case.name
            );
            assert_eq!(
                stand_ins.first().map(|s| s.verdict),
                case.stand_in,
                "{} stand-in",
                case.name
            );
            assert_eq!(
                stand_ins.len(),
                usize::from(case.stand_in.is_some()),
                "{} count",
                case.name
            );
        }
    }

    #[test]
    fn promotion_neutralises_the_demoted_masters_new_pending_slot() {
        use clank_core::Verdict::Continue;
        use clank_core::vocab::CommitGateState;

        let before = tiers(&["incoming-master"], &[], &[]);
        let after = tiers(&["old-master"], &[], &[]);
        let mut target = gate_target(false, &[("incoming-master", Continue)]);
        let stand_ins = plan_roster_stand_ins(
            before,
            after.clone(),
            RosterTransitionPolicy::PreserveGate,
            std::slice::from_mut(&mut target),
        )
        .unwrap();

        assert_eq!(target_gate(&target, &after), CommitGateState::Continued);
        assert_eq!(stand_ins.len(), 1);
        assert_eq!(stand_ins[0].author, label("old-master"));
        assert_eq!(stand_ins[0].verdict, Continue);
    }

    #[test]
    fn tier_move_uses_the_same_preservation_seam() {
        use clank_core::Verdict::Continue;
        use clank_core::vocab::CommitGateState;

        let before = tiers(&["alice"], &[], &[]);
        let after = tiers(&[], &["alice"], &[]);
        let mut target = gate_target(true, &[]);
        let stand_ins = plan_roster_stand_ins(
            before,
            after.clone(),
            RosterTransitionPolicy::PreserveGate,
            std::slice::from_mut(&mut target),
        )
        .unwrap();

        assert_eq!(target_gate(&target, &after), CommitGateState::Continued);
        assert_eq!(stand_ins.len(), 1);
        assert_eq!(stand_ins[0].verdict, Continue);
    }

    #[test]
    fn swap_deliberately_reopens_for_the_replacement() {
        use clank_core::Verdict::Continue;
        use clank_core::vocab::CommitGateState;

        let before = tiers(&["out"], &[], &[]);
        let after = tiers(&["into"], &[], &[]);
        let mut target = gate_target(false, &[("out", Continue)]);
        let stand_ins = plan_roster_stand_ins(
            before,
            after.clone(),
            RosterTransitionPolicy::RequireFreshReview,
            std::slice::from_mut(&mut target),
        )
        .unwrap();

        assert!(stand_ins.is_empty());
        assert_eq!(target_gate(&target, &after), CommitGateState::Unreviewed);
    }

    #[test]
    fn existing_real_verdict_is_never_replaced() {
        use clank_core::Verdict::{Continue, RequestChanges};
        use clank_core::vocab::CommitGateState;

        let before = tiers(&["alice"], &[], &[]);
        let after = tiers(&["alice", "returning"], &[], &[]);
        let mut target = gate_target(false, &[("alice", Continue), ("returning", RequestChanges)]);
        let stand_ins = plan_roster_stand_ins(
            before,
            after.clone(),
            RosterTransitionPolicy::PreserveGate,
            std::slice::from_mut(&mut target),
        )
        .unwrap();

        assert!(stand_ins.is_empty());
        assert_eq!(
            target_gate(&target, &after),
            CommitGateState::ChangesRequested
        );
        assert_eq!(
            target
                .reviews
                .iter()
                .filter(|review| review.author == label("returning"))
                .count(),
            1
        );
    }

    #[test]
    fn stand_in_write_is_marked_and_never_clobbers_real_feedback() {
        let repo = tempfile::tempdir().unwrap();
        let sha = clank_core::ids::CommitSha::parse(&"b".repeat(40)).unwrap();
        let stand_in = RosterStandIn {
            author: label("new"),
            sha: sha.clone(),
            verdict: clank_core::Verdict::Continue,
        };
        let path = write_stand_in_noclobber(repo.path(), &stand_in, std::slice::from_ref(&sha))
            .unwrap()
            .unwrap();
        let synthetic = std::fs::read_to_string(&path).unwrap();
        assert!(clank_core::feedback_body::is_roster_stand_in(&synthetic));

        std::fs::write(&path, "REQUEST_CHANGES real review\n").unwrap();
        assert!(
            write_stand_in_noclobber(repo.path(), &stand_in, std::slice::from_ref(&sha))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "REQUEST_CHANGES real review\n"
        );
    }

    #[test]
    fn latest_gate_targets_never_backfills_older_commits() {
        let repo = tempfile::tempdir().unwrap();
        let old = clank_core::ids::CommitSha::parse(&"1".repeat(40)).unwrap();
        let latest = clank_core::ids::CommitSha::parse(&"2".repeat(40)).unwrap();
        let mut state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        state.fold.plans.insert(
            clank_core::ids::PlanKey::parse("example").unwrap(),
            clank_core::repo_state::PlanState {
                commits: vec![
                    clank_core::repo_state::PlanTimelineEvent {
                        sha: old,
                        ts: 1,
                        touched_plan: true,
                        touched_code: false,
                    },
                    clank_core::repo_state::PlanTimelineEvent {
                        sha: latest.clone(),
                        ts: 2,
                        touched_plan: false,
                        touched_code: true,
                    },
                ],
            },
        );

        let targets = latest_gate_targets(repo.path(), &state);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].sha, latest);
        assert_eq!(targets[0].touched_plan, vec![false]);
    }

    #[test]
    fn roster_config_has_one_raw_write_choke_point() {
        let source = include_str!("agent.rs");
        let raw_writer = concat!("write_repo_config", "_raw(");
        assert_eq!(
            source.matches(raw_writer).count(),
            2,
            "the raw writer must appear only at its definition and inside write_repo_transition"
        );
    }

    // ── agent add (inline) ───────────────────────────────────

    #[test]
    fn add_repo_roster_agent_inserts_with_role() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        add_repo_roster_agent(repo.path(), &codex, desc(Tool::Codex), RosterRole::Commit).unwrap();
        let parsed = read_repo(repo.path());
        assert_eq!(parsed.agents[&codex].tool, Tool::Codex);
        assert_eq!(parsed.agents[&codex].role, RosterRole::Commit);
        // master untouched.
        assert_eq!(
            parsed.agents[&AgentLabel::parse("claude").unwrap()].role,
            RosterRole::Master
        );
    }

    #[test]
    fn add_repo_roster_agent_gate_role() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let ruthless = AgentLabel::parse("ruthless").unwrap();
        add_repo_roster_agent(repo.path(), &ruthless, desc(Tool::Claude), RosterRole::Gate)
            .unwrap();
        let parsed = read_repo(repo.path());
        assert_eq!(parsed.agents[&ruthless].role, RosterRole::Gate);
    }

    #[test]
    fn add_repo_roster_agent_rejects_duplicate_label() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": {
                "claude": { "tool": "claude", "role": "master" },
                "codex": { "tool": "codex", "role": "commit" }
            } }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        let err = add_repo_roster_agent(repo.path(), &codex, desc(Tool::Codex), RosterRole::Gate)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already on this repo's roster"));
        assert!(msg.contains("clank agent remove"));
    }

    // ── agent add (by name, copy-down) ───────────────────────

    fn seed_user_config(home: &Path, agents: &[(&str, Tool)]) {
        let mut cfg = UserConfigFile::default();
        for (l, t) in agents {
            cfg.agents.insert(AgentLabel::parse(l).unwrap(), desc(*t));
        }
        std::fs::create_dir_all(home.join(".clank")).unwrap();
        std::fs::write(
            home.join(".clank/config.json"),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn add_by_name_copies_description_from_library() {
        let home = tempfile::tempdir().unwrap();
        seed_user_config(home.path(), &[("ruthless", Tool::Claude)]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let ruthless = AgentLabel::parse("ruthless").unwrap();
        add_repo_roster_agent_by_name(repo.path(), Some(home.path()), &ruthless, RosterRole::Gate)
            .unwrap();
        let parsed = read_repo(repo.path());
        assert_eq!(parsed.agents[&ruthless].tool, Tool::Claude);
        assert_eq!(parsed.agents[&ruthless].role, RosterRole::Gate);
    }

    #[test]
    fn add_by_name_errors_when_not_in_library() {
        let home = tempfile::tempdir().unwrap();
        seed_user_config(home.path(), &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let phantom = AgentLabel::parse("phantom").unwrap();
        let err = add_repo_roster_agent_by_name(
            repo.path(),
            Some(home.path()),
            &phantom,
            RosterRole::Commit,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown agent `phantom`"));
        assert!(msg.contains("--tool"));
        assert!(msg.contains("--global"));
    }

    #[test]
    fn add_by_name_rejects_duplicate_label() {
        let home = tempfile::tempdir().unwrap();
        seed_user_config(home.path(), &[("codex", Tool::Codex)]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": {
                "claude": { "tool": "claude", "role": "master" },
                "codex": { "tool": "codex", "role": "commit" }
            } }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        let err =
            add_repo_roster_agent_by_name(repo.path(), Some(home.path()), &codex, RosterRole::Gate)
                .unwrap_err();
        assert!(format!("{err:#}").contains("already on this repo's roster"));
    }

    // ── agent promote ─────────────────────────────────────

    #[test]
    fn set_repo_master_sets_and_demotes_previous() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": {
                "claude": { "tool": "claude", "role": "master" },
                "codex": { "tool": "codex", "role": "commit" }
            } }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        set_repo_master(repo.path(), &codex).unwrap();
        let parsed = read_repo(repo.path());
        assert_eq!(parsed.agents[&codex].role, RosterRole::Master);
        // Previous master demoted to commit.
        assert_eq!(
            parsed.agents[&AgentLabel::parse("claude").unwrap()].role,
            RosterRole::Commit
        );
    }

    #[test]
    fn set_repo_master_errors_when_not_on_roster() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let phantom = AgentLabel::parse("phantom").unwrap();
        let err = set_repo_master(repo.path(), &phantom).unwrap_err();
        assert!(format!("{err:#}").contains("not on this repo's roster"));
    }

    #[test]
    fn set_repo_master_no_op_when_already_master() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let claude = AgentLabel::parse("claude").unwrap();
        set_repo_master(repo.path(), &claude).unwrap();
        let parsed = read_repo(repo.path());
        assert_eq!(parsed.agents[&claude].role, RosterRole::Master);
    }

    // ── agent set-review ─────────────────────────────────────

    #[test]
    fn set_repo_review_flips_tier_both_ways_preserving_other_fields() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": {
                "claude": { "tool": "claude", "role": "master" },
                "codex": { "tool": "codex", "role": "commit", "initial_prompt": "go" }
            } }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        // commit -> gate
        set_repo_review(repo.path(), &codex, ReviewKind::Gate).unwrap();
        let parsed = read_repo(repo.path());
        assert_eq!(parsed.agents[&codex].role, RosterRole::Gate);
        // other fields preserved, master untouched
        assert_eq!(parsed.agents[&codex].initial_prompt.as_deref(), Some("go"));
        assert_eq!(
            parsed.agents[&AgentLabel::parse("claude").unwrap()].role,
            RosterRole::Master
        );
        // gate -> commit
        set_repo_review(repo.path(), &codex, ReviewKind::Commit).unwrap();
        assert_eq!(
            read_repo(repo.path()).agents[&codex].role,
            RosterRole::Commit
        );
    }

    #[test]
    fn set_repo_review_refuses_master() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let claude = AgentLabel::parse("claude").unwrap();
        let err = set_repo_review(repo.path(), &claude, ReviewKind::Gate).unwrap_err();
        let msg = format!("{err:#}");
        // Points only at the command that exists (promote), never `demote`.
        assert!(
            msg.contains("is the master") && msg.contains("clank agent promote"),
            "{msg}"
        );
        assert!(
            !msg.contains("agent demote"),
            "must not suggest a nonexistent command: {msg}"
        );
        // Master role unchanged.
        assert_eq!(
            read_repo(repo.path()).agents[&claude].role,
            RosterRole::Master
        );
    }

    #[test]
    fn set_repo_review_errors_when_not_on_roster() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let phantom = AgentLabel::parse("phantom").unwrap();
        let err = set_repo_review(repo.path(), &phantom, ReviewKind::Commit).unwrap_err();
        assert!(format!("{err:#}").contains("not on this repo's roster"));
    }

    #[test]
    fn set_repo_review_no_op_when_already_at_tier() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": {
                "claude": { "tool": "claude", "role": "master" },
                "codex": { "tool": "codex", "role": "commit" }
            } }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        set_repo_review(repo.path(), &codex, ReviewKind::Commit).unwrap();
        assert_eq!(
            read_repo(repo.path()).agents[&codex].role,
            RosterRole::Commit
        );
    }

    // ── agent remove ─────────────────────────────────────────

    #[test]
    fn remove_repo_agent_drops_from_roster() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": {
                "claude": { "tool": "claude", "role": "master" },
                "codex": { "tool": "codex", "role": "commit" }
            } }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        remove_repo_agent(repo.path(), &codex).unwrap();
        let parsed = read_repo(repo.path());
        assert!(!parsed.agents.contains_key(&codex));
        assert!(
            parsed
                .agents
                .contains_key(&AgentLabel::parse("claude").unwrap())
        );
    }

    #[test]
    fn remove_repo_agent_errors_when_not_on_roster() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );
        let phantom = AgentLabel::parse("phantom").unwrap();
        let err = remove_repo_agent(repo.path(), &phantom).unwrap_err();
        assert!(format!("{err:#}").contains("not on this repo's roster"));
    }

    #[test]
    fn remove_global_agent_scrubs_team_templates() {
        use crate::cli::teams_config::{TeamMember, TeamRef, TeamRoster};
        let home = tempfile::tempdir().unwrap();
        let mut cfg = UserConfigFile::default();
        cfg.agents
            .insert(AgentLabel::parse("codex").unwrap(), desc(Tool::Codex));
        let mut dev: TeamRoster = BTreeMap::new();
        // A ref by key, plus an aliased ref pointing at the same
        // library agent — both must be scrubbed.
        dev.insert(
            AgentLabel::parse("codex").unwrap(),
            TeamMember::Ref(TeamRef {
                agent: None,
                role: RosterRole::Commit,
            }),
        );
        dev.insert(
            AgentLabel::parse("reviewer").unwrap(),
            TeamMember::Ref(TeamRef {
                agent: Some(AgentLabel::parse("codex").unwrap()),
                role: RosterRole::Gate,
            }),
        );
        // An inline member is self-contained — it must survive.
        dev.insert(
            AgentLabel::parse("keep").unwrap(),
            TeamMember::Inline(RosterAgent::from_description(
                desc(Tool::Claude),
                RosterRole::Master,
            )),
        );
        cfg.teams.insert("dev".to_string(), dev);
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        std::fs::write(
            home.path().join(".clank/config.json"),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();

        let codex = AgentLabel::parse("codex").unwrap();
        remove_global_agent(home.path(), &codex).unwrap();
        let back = read_user_config(home.path()).unwrap();
        assert!(!back.agents.contains_key(&codex));
        let dev = back.teams.get("dev").unwrap();
        assert!(!dev.contains_key(&codex), "ref by key must be scrubbed");
        assert!(
            !dev.contains_key(&AgentLabel::parse("reviewer").unwrap()),
            "aliased ref to the removed agent must be scrubbed"
        );
        assert!(
            dev.contains_key(&AgentLabel::parse("keep").unwrap()),
            "inline member must survive"
        );
    }

    #[test]
    fn compose_launch_appends_initial_prompt_when_set() {
        let s = claude_session();
        let c = compose_launch(
            Path::new("/repo"),
            &lbl("tester"),
            &s,
            None,
            Some("custom prompt"),
        );
        assert_eq!(
            c.args.last().map(|s| s.as_str()),
            Some("custom prompt"),
            "trailing arg should be the prompt; got: {:?}",
            c.args
        );
    }

    #[test]
    fn session_name_leads_with_the_worktree_then_the_label() {
        // A worktree, a clone and a plain checkout all name themselves
        // by their directory — the same string the zellij tab carries.
        assert_eq!(
            session_display_name(
                Path::new("/Users/x/src/frostsnap-ci/.clank/worktrees/recovery-scan"),
                &lbl("kimi")
            ),
            "recovery-scan · kimi"
        );
        assert_eq!(
            session_display_name(Path::new("/Users/x/src/clank"), &lbl("claude")),
            "clank · claude"
        );
        // The label is what keeps a shared worktree's sessions apart:
        // master and every reviewer live in the same directory.
        assert_ne!(
            session_display_name(Path::new("/r/wt"), &lbl("claude")),
            session_display_name(Path::new("/r/wt"), &lbl("codex"))
        );
    }

    #[test]
    fn only_claude_takes_the_name_as_a_flag() {
        // Verified against the installed binaries: claude has
        // top-level `-n, --name`; opencode's `--title` exists only on
        // its non-interactive `run`; codex has none.
        for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode, Tool::Grok] {
            let mut args = Vec::new();
            set_session_name(tool, &mut args, "wt · a");
            let prompt = name_led_prompt(tool, "wt · a", "do the thing".to_string());
            if tool == Tool::Claude {
                assert_eq!(args, vec!["-n".to_string(), "wt · a".to_string()]);
                // Named outright, so its prompt is left alone.
                assert_eq!(prompt, "do the thing");
            } else {
                assert!(args.is_empty(), "{tool:?} has no name flag to pass");
                // Derived from the opening message, so the name leads.
                assert!(
                    prompt.starts_with("wt · a"),
                    "{tool:?} prompt must lead with the name: {prompt}"
                );
                assert!(prompt.ends_with("do the thing"));
            }
        }
    }

    #[test]
    fn a_fork_is_named_for_the_fork_not_the_session_it_came_from() {
        // The headline case: `clank fork` is where a session most
        // needs its own name, because a forked session otherwise
        // inherits the SOURCE's — the one name guaranteed to be wrong.
        let wt = Path::new("/repo/.clank/worktrees/recovery-scan");
        let orient = "You are `kimi` in worktree `recovery-scan`…";

        // claude takes it as a flag, so the prompt is untouched.
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Claude,
            from_session: Some("src-session".into()),
            prompt: orient.into(),
        };
        let c = compose_fork_launch(
            &lbl("kimi"),
            &spec,
            &desc_with(Tool::Claude, None),
            wt,
            None,
        );
        let n = c
            .args
            .iter()
            .position(|a| a == "-n")
            .expect("fork is named");
        assert_eq!(c.args[n + 1], "recovery-scan · kimi");
        assert!(c.args.iter().any(|a| a == orient));

        // codex and opencode derive it, so the name must LEAD — the
        // orientation prompt opens "You are `kimi` in worktree …",
        // which buries the fork name where truncation cuts it off.
        for tool in [Tool::Codex, Tool::OpenCode] {
            let spec = crate::cli::fork::ForkSpec {
                tool,
                from_session: Some("src-session".into()),
                prompt: orient.into(),
            };
            let c = compose_fork_launch(&lbl("kimi"), &spec, &desc_with(tool, None), wt, None);
            let prompt = c.args.last().expect("a prompt");
            assert!(
                prompt.starts_with("recovery-scan · kimi"),
                "{tool:?} fork prompt must lead with the fork name: {prompt}"
            );
            assert!(
                prompt.ends_with(orient),
                "orientation is preserved: {prompt}"
            );
            assert!(
                !c.args.iter().any(|a| a == "-n"),
                "{tool:?} has no name flag"
            );
        }
    }

    #[test]
    fn a_resumed_session_is_named_again() {
        // Deliberate: nothing can read a session's current name back,
        // so setting it only on first launch would leave every session
        // created before this permanently mis-titled — which is the
        // reported problem, not a hypothetical.
        let s = claude_session();
        let c = compose_launch(Path::new("/repo"), &lbl("tester"), &s, None, None);
        let n = c
            .args
            .iter()
            .position(|a| a == "-n")
            .expect("named on resume");
        assert_eq!(c.args[n + 1], "repo · tester");
    }

    #[test]
    fn compose_launch_omits_prompt_when_none() {
        let s = claude_session();
        let c = compose_launch(Path::new("/repo"), &lbl("tester"), &s, None, None);
        // No prompt is appended. The trailing pair is the session
        // NAME clank always sets for claude, so "last arg" no longer
        // distinguishes prompt from no-prompt — assert on the absence.
        assert_eq!(
            c.args,
            vec![
                "--resume",
                "aaaaaaaa-1111-2222-3333-444444444444",
                "-n",
                "repo · tester"
            ],
            "no trailing prompt should be appended; got: {:?}",
            c.args
        );
    }

    #[test]
    fn compose_launch_codex_prompt_is_final_positional_after_cd() {
        // For codex, --cd <repo> comes from session_restore_args
        // BEFORE the prompt. The prompt is the final positional.
        let s = codex_session();
        let c = compose_launch(Path::new("/repo"), &lbl("tester"), &s, None, Some("ack"));
        // Expected: ["resume", "<id>", "--cd", "/repo", "ack"]
        // codex has no name flag, so the prompt carries the name.
        let prompt = "repo · tester — ack";
        assert_eq!(c.args.last().map(|s| s.as_str()), Some(prompt));
        // --cd <repo> appears before the prompt.
        let cd_pos = c.args.iter().position(|s| s == "--cd").unwrap();
        let prompt_pos = c.args.iter().position(|s| s == prompt).unwrap();
        assert!(
            cd_pos < prompt_pos,
            "--cd must come before prompt; argv: {:?}",
            c.args
        );
    }

    #[test]
    fn compose_launch_grok_auto_start_carries_the_arming_prompt() {
        // End-to-end shape of the grok-first-turn-orchestration fix:
        // a grok session started under effective auto-on composes a
        // launch whose prompt IS the arming instruction (grok has no
        // hook to orchestrate it after a bare ack).
        let session = grok_session();
        let prompt = resolve_initial_prompt(None, AutoMode::On, session.tool)
            .expect("auto-on default prompt");
        let c = compose_launch(
            Path::new("/repo"),
            &lbl("tester"),
            &session,
            None,
            Some(&prompt),
        );
        let last = c.args.last().map(|s| s.as_str()).unwrap_or("");
        assert!(
            last.contains("clank wait") && last.contains("background"),
            "the composed grok launch must instruct arming: {:?}",
            c.args
        );
    }

    // ── resolve_initial_prompt policy tests ────────────────────

    #[test]
    fn resolve_initial_prompt_uses_declaration_field_when_set() {
        let out = resolve_initial_prompt(Some("foo"), AutoMode::Off, Tool::Claude);
        assert_eq!(out, Some("foo".to_string()));
    }

    #[test]
    fn resolve_initial_prompt_uses_default_when_auto_on_and_declaration_unset() {
        // Pinned: exact equality against the constant so a future
        // tweak to DEFAULT_AUTO_PROMPT fails the test deliberately.
        // Claude, codex AND opencode get the bare ack — an
        // orchestrator (stop hook / clank plugin) reacts to the
        // turn ending.
        for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
            let out = resolve_initial_prompt(None, AutoMode::On, tool);
            assert_eq!(out, Some("Session resumed.".to_string()));
        }
    }

    #[test]
    fn resolve_initial_prompt_grok_default_carries_the_arming_instruction() {
        // grok-first-turn-orchestration: grok has NO clank hook, so
        // the bare ack ends its first turn with nothing armed and it
        // never wakes. Its default must instruct the arming. Pinned
        // exactly, like the claude/codex constant.
        let out = resolve_initial_prompt(None, AutoMode::On, Tool::Grok);
        assert_eq!(
            out,
            Some(
                "Session resumed. Arm your clank work loop now: run `clank wait` as a \
                 background terminal command (background: true), then end your turn. Its \
                 completion wakes you with work items."
                    .to_string()
            )
        );
    }

    #[test]
    fn resolve_initial_prompt_returns_none_when_auto_off_and_declaration_unset() {
        for tool in [Tool::Claude, Tool::Codex, Tool::Grok] {
            let out = resolve_initial_prompt(None, AutoMode::Off, tool);
            assert_eq!(out, None);
        }
    }

    #[test]
    fn resolve_initial_prompt_declaration_wins_over_auto_default() {
        // Including for grok: an explicit prompt overrides the arming
        // default too.
        for tool in [Tool::Claude, Tool::Codex, Tool::Grok] {
            let out = resolve_initial_prompt(Some("custom"), AutoMode::On, tool);
            assert_eq!(out, Some("custom".to_string()));
        }
    }

    #[test]
    fn resolve_initial_prompt_empty_declaration_string_disables_prompt() {
        // Ruthless 0fe1567 pin: Some("") is the explicit-disable
        // escape hatch. Without this, the only way to opt out of
        // the auto_mode default would be to disable auto_mode
        // itself — coupling two unrelated concerns. Grok included.
        for tool in [Tool::Claude, Tool::Codex, Tool::Grok] {
            let out = resolve_initial_prompt(Some(""), AutoMode::On, tool);
            assert_eq!(out, None);
        }
    }

    // ── compose_bootstrap_launch policy tests ─────────────────
    // Plan: agent-start-bootstraps-missing-skeleton.

    #[test]
    fn opencode_prompts_ride_behind_the_prompt_flag() {
        // opencode's TUI positional is a PROJECT PATH — a prompt
        // pushed as the trailing positional would be read as a
        // directory (opencode-agent-tool M2). Every launch mode's
        // prompt must use `--prompt`.
        let desc = desc_with(
            Tool::OpenCode,
            Some(LaunchConfig {
                command: None,
                args: vec!["--model".into(), "moonshotai/kimi-k3".into()],
                env: Default::default(),
            }),
        );
        let c =
            compose_bootstrap_launch(Path::new("/repo"), &label("kimi"), &desc).expect("compose");
        assert_eq!(c.program, "opencode");
        assert_eq!(
            c.args,
            vec![
                "--model".to_string(),
                "moonshotai/kimi-k3".to_string(),
                "--prompt".to_string(),
                // opencode's --title is `run`-only, so the interactive
                // launch derives its title from this prompt.
                "repo · kimi — Run `clank as kimi` to bind this session.".to_string(),
            ]
        );
    }

    #[test]
    fn opencode_fork_resumes_a_copy_under_the_fork_flag() {
        // `opencode --session <src> --fork` branches the source under
        // a fresh ses_ id (verified live, opencode-agent-tool M2); the
        // orientation prompt rides behind --prompt like every other
        // opencode launch.
        let desc = desc_with(Tool::OpenCode, None);
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::OpenCode,
            from_session: Some("ses_039d60658ffe0RPgue3noZ0Qqf".into()),
            prompt: "orient".into(),
        };
        let c = compose_fork_launch(
            &lbl("tester"),
            &spec,
            &desc,
            Path::new("/repo/.clank/worktrees/x"),
            None,
        );
        assert_eq!(c.program, "opencode");
        assert_eq!(
            c.args,
            vec![
                "--session",
                "ses_039d60658ffe0RPgue3noZ0Qqf",
                "--fork",
                "--prompt",
                // No name flag: the fork's name leads the prompt.
                "x · tester — orient",
            ]
        );
    }

    #[test]
    fn opencode_launches_disable_the_claude_compat_skill_scan() {
        // The claude-flavored role skills teach background-wait
        // arming — wrong for a plugin-driven tool — and WIN
        // opencode's name dedupe over the native copies setup
        // installs (observed live). Every opencode launch mode
        // disables the compat scan; a deliberate launch.env override
        // wins.
        let desc = desc_with(Tool::OpenCode, None);
        let c =
            compose_bootstrap_launch(Path::new("/repo"), &label("kimi"), &desc).expect("compose");
        assert_eq!(
            c.env_overrides.get("OPENCODE_DISABLE_CLAUDE_CODE_SKILLS"),
            Some(&"1".to_string())
        );
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS".to_string(),
            "0".to_string(),
        );
        let desc = desc_with(
            Tool::OpenCode,
            Some(LaunchConfig {
                command: None,
                args: vec![],
                env,
            }),
        );
        let c =
            compose_bootstrap_launch(Path::new("/repo"), &label("kimi"), &desc).expect("compose");
        assert_eq!(
            c.env_overrides.get("OPENCODE_DISABLE_CLAUDE_CODE_SKILLS"),
            Some(&"0".to_string())
        );
        // Other tools carry no opencode default.
        let desc = desc_with(Tool::Claude, None);
        let c = compose_bootstrap_launch(Path::new("/repo"), &label("phantom"), &desc)
            .expect("compose");
        assert!(c.env_overrides.is_empty());
    }

    #[test]
    fn launches_scrub_every_inherited_identity_var() {
        // The child env must not carry ANOTHER agent's identity
        // (observed live: opencode under a claude shell bound the
        // claude session — opencode-agent-tool M1). Asserted at the
        // Command level via get_envs: scrubbed vars map to None
        // (explicit removal), and a deliberate launch.env override
        // applied after still wins.
        let mut cmd = std::process::Command::new("true");
        scrub_inherited_identity(&mut cmd);
        cmd.env("CLAUDE_CODE_SESSION_ID", "deliberate");
        let envs: std::collections::BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        for var in [
            "CODEX_THREAD_ID",
            "OPENCODE_SESSION_ID",
            "GROK_AGENT",
            "CLANK_AGENT",
        ] {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(var)),
                Some(&None),
                "{var} must be removed"
            );
        }
        assert_eq!(
            envs.get(std::ffi::OsStr::new("CLAUDE_CODE_SESSION_ID")),
            Some(&Some("deliberate".into()))
        );
    }

    fn desc_with(tool: Tool, launch: Option<LaunchConfig>) -> AgentDescription {
        AgentDescription {
            tool,
            launch,
            initial_prompt: None,
        }
    }

    #[test]
    fn bootstrap_uses_tool_from_description() {
        let desc = desc_with(
            Tool::Claude,
            Some(LaunchConfig {
                command: None,
                args: vec!["--skill".into(), "ruthless".into()],
                env: Default::default(),
            }),
        );
        let composed = compose_bootstrap_launch(Path::new("/repo"), &label("phantom"), &desc)
            .expect("compose");
        assert_eq!(composed.program, "claude");
        assert_eq!(
            composed.args,
            vec![
                "--skill".to_string(),
                "ruthless".to_string(),
                // claude takes the name outright, so its prompt is
                // left alone.
                "-n".to_string(),
                "repo · phantom".to_string(),
                "Run `clank as phantom` to bind this session.".to_string(),
            ]
        );
    }

    #[test]
    fn bootstrap_prefers_launch_command_over_tool() {
        let desc = desc_with(
            Tool::Claude,
            Some(LaunchConfig {
                command: Some("my-claude-wrapper".into()),
                args: vec![],
                env: Default::default(),
            }),
        );
        let composed = compose_bootstrap_launch(Path::new("/repo"), &label("phantom"), &desc)
            .expect("compose");
        // launch.command wins; tool=claude is only the fallback.
        assert_eq!(composed.program, "my-claude-wrapper");
    }

    #[test]
    fn bootstrap_falls_back_to_tool_when_no_launch_command() {
        // No launch profile at all → program is the tool name.
        let desc = desc_with(Tool::Codex, None);
        let composed = compose_bootstrap_launch(Path::new("/repo"), &label("phantom"), &desc)
            .expect("compose");
        assert_eq!(composed.program, "codex");
        assert_eq!(
            composed.args,
            // No name flag for this tool, so the title is DERIVED
            // from the opening message and the name has to lead it.
            vec!["repo · phantom — Run `clank as phantom` to bind this session.".to_string()]
        );
    }

    #[test]
    fn bootstrap_bind_prompt_is_pinned_verbatim() {
        // Pinned: exact equality so a future wording tweak fails
        // the test deliberately.
        let s = bootstrap_bind_prompt(&label("phantom"));
        assert_eq!(s, "Run `clank as phantom` to bind this session.");
    }

    #[test]
    fn resolve_initial_prompt_empty_declaration_string_disables_prompt_under_auto_off() {
        // Declaration is authoritative regardless of auto_mode.
        let out = resolve_initial_prompt(Some(""), AutoMode::Off, Tool::Claude);
        assert_eq!(out, None);
    }

    #[test]
    fn agent_row_bound_uses_session_tool() {
        let session = make_session();
        let row = AgentRow::from_join(
            &label("alice"),
            "reviewer",
            "commit",
            Tool::Codex, // description tool (overridden by session when bound)
            Some(&session),
        );
        assert_eq!(row.label, "alice");
        assert_eq!(row.role, "reviewer");
        assert_eq!(row.review, "commit");
        assert!(row.bound);
        assert_eq!(row.tool.as_deref(), Some("claude")); // from session
        assert!(row.session_id.is_some());
    }

    #[test]
    fn agent_row_unbound_uses_description_tool() {
        let row = AgentRow::from_join(&label("alice"), "reviewer", "gate", Tool::Claude, None);
        assert!(!row.bound);
        assert_eq!(row.review, "gate");
        assert_eq!(row.tool.as_deref(), Some("claude"));
        assert!(row.session_id.is_none());
    }

    #[test]
    fn agent_row_master_has_no_review_tier() {
        let row = AgentRow::from_join(&label("boss"), "master", "—", Tool::Claude, None);
        assert_eq!(row.role, "master");
        assert_eq!(row.review, "—");
    }

    #[test]
    fn codex_launch_under_stale_claude_roster_still_records_trust() {
        // codex ff3ca44: the trust step keys on the tool the composed
        // launch ACTUALLY runs (fork spec / bound session), never the
        // roster description — a codex fork spec under a stale claude
        // roster entry must still record trust, and a claude launch
        // must write nothing.
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
        // The spec's tool is what flows to the trust step (captured in
        // the same match arm that picks the composer).
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Codex,
            from_session: None,
            prompt: "orient".into(),
        };
        pre_launch_codex_trust(spec.tool, repo.path(), Some(home.path()));
        let cfg = home.path().join(".codex/config.toml");
        let body = std::fs::read_to_string(&cfg).unwrap();
        assert!(body.contains("trust_level = \"trusted\""), "{body}");

        // A claude launch writes nothing.
        let home2 = tempfile::tempdir().unwrap();
        pre_launch_codex_trust(Tool::Claude, repo.path(), Some(home2.path()));
        assert!(!home2.path().join(".codex/config.toml").exists());
    }

    #[test]
    fn codex_trust_appends_preserves_and_never_overrides() {
        // codex-trust-at-launch: the ensure step mirrors codex's own
        // Yes-answer write, appends without touching existing content,
        // and NEVER rewrites an existing projects entry (a prior user
        // decision — possibly an explicit distrust).
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join(".codex/config.toml");
        let root_a = std::path::Path::new("/repos/alpha");
        let root_b = std::path::Path::new("/repos/beta");

        // Creates the file when absent.
        ensure_codex_project_trust(home.path(), root_a).unwrap();
        let body = std::fs::read_to_string(&cfg).unwrap();
        assert!(body.contains("[projects.\"/repos/alpha\"]"));
        assert!(body.contains("trust_level = \"trusted\""));

        // Idempotent: a second ensure adds nothing.
        ensure_codex_project_trust(home.path(), root_a).unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);

        // Existing content (including an explicit DISTRUST for beta)
        // survives verbatim; beta's entry is never overridden.
        let seeded = format!(
            "model = \"gpt-5.5\"\n\n[projects.\"/repos/beta\"]\ntrust_level = \"untrusted\"\n"
        );
        std::fs::write(&cfg, &seeded).unwrap();
        ensure_codex_project_trust(home.path(), root_b).unwrap();
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            seeded,
            "an existing entry is a prior user decision — untouched"
        );
        ensure_codex_project_trust(home.path(), root_a).unwrap();
        let body = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            body.starts_with(&seeded),
            "existing file preserved verbatim"
        );
        assert!(body.contains("[projects.\"/repos/alpha\"]\ntrust_level = \"trusted\""));
    }

    #[test]
    fn grok_bootstrap_launch_carries_trust_before_the_bind_prompt() {
        // codex 703312b: a registered grok agent with NO bound session
        // and no fork spec reaches compose_bootstrap_launch — the trust
        // flag must ride that path too or the first `clank agent start`
        // hits grok's folder-trust prompt.
        let desc = AgentDescription {
            tool: Tool::Grok,
            launch: None,
            initial_prompt: None,
        };
        let c = compose_bootstrap_launch(Path::new("/repo"), &label("fresh-grok"), &desc)
            .expect("compose");
        assert_eq!(c.program, "grok");
        assert_eq!(c.args.first().map(|s| s.as_str()), Some("--trust"));
        assert_eq!(c.args.len(), 2, "trust + bind prompt only");
    }

    /// A fixture HOME in which `sid` IS resumable for `tool`, so a
    /// test can assert the resume argv without depending on the real
    /// `$HOME`. Mirrors each tool's real store layout — the probe
    /// walks these paths for real.
    fn home_with_session(tool: Tool, sid: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        match tool {
            Tool::Claude => {
                let d = h.join(".claude/projects/some-project");
                std::fs::create_dir_all(&d).unwrap();
                std::fs::write(d.join(format!("{sid}.jsonl")), "{}\n").unwrap();
            }
            Tool::Codex => {
                let d = h.join(".codex/sessions/2026/08/13");
                std::fs::create_dir_all(&d).unwrap();
                std::fs::write(d.join(format!("rollout-{sid}.jsonl")), "{}\n").unwrap();
            }
            Tool::Grok => {
                std::fs::create_dir_all(h.join(".grok/sessions/cwd-group").join(sid)).unwrap();
            }
            Tool::OpenCode => {}
        }
        home
    }

    #[test]
    fn grok_fork_resumes_and_branches_with_trust() {
        // grok-first-class: fork = --resume <sid> --fork-session (grok
        // names the child; binding lands later via `clank as`), plus
        // the grok-only --trust, then the orientation prompt. Without
        // a source session: fresh launch, still trusted + oriented.
        let desc = AgentDescription {
            tool: Tool::Grok,
            launch: None,
            initial_prompt: None,
        };
        let wt = std::path::Path::new("/repo/.clank/worktrees/x");
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Grok,
            from_session: Some("cccccccc-1111-2222-3333-444444444444".into()),
            prompt: "orient".into(),
        };
        let home = home_with_session(Tool::Grok, "cccccccc-1111-2222-3333-444444444444");
        let c = compose_fork_launch(&lbl("tester"), &spec, &desc, wt, Some(home.path()));
        assert_eq!(c.program, "grok");
        assert_eq!(
            c.args,
            vec![
                "--resume",
                "cccccccc-1111-2222-3333-444444444444",
                "--fork-session",
                "--trust",
                "x · tester — orient",
            ]
        );

        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Grok,
            from_session: None,
            prompt: "orient".into(),
        };
        let c = compose_fork_launch(&lbl("tester"), &spec, &desc, wt, None);
        assert_eq!(c.args, vec!["--trust", "x · tester — orient"]);
    }

    #[test]
    fn fork_launch_without_source_session_bootstraps_with_orientation() {
        // fork-robustness: a spec with no from_session launches a FRESH
        // session that still carries the orientation prompt — no
        // --resume/fork argv, same shape as the bootstrap launch.
        let desc = AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        };
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Claude,
            from_session: None,
            prompt: "You are `claude` in worktree `x`…".into(),
        };
        let wt = std::path::Path::new("/repo/.clank/worktrees/x");
        let c = compose_fork_launch(&lbl("tester"), &spec, &desc, wt, None);
        assert_eq!(c.program, "claude");
        assert_eq!(
            c.args,
            vec!["-n", "x · tester", "You are `claude` in worktree `x`…"]
        );

        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Codex,
            from_session: None,
            prompt: "orient".into(),
        };
        let desc = AgentDescription {
            tool: Tool::Codex,
            launch: None,
            initial_prompt: None,
        };
        let c = compose_fork_launch(&lbl("tester"), &spec, &desc, wt, None);
        assert_eq!(c.program, "codex");
        assert_eq!(
            c.args,
            vec!["x · tester — orient"],
            "no fork/-C argv without a source session"
        );
    }

    #[test]
    fn fork_launch_falls_back_fresh_when_the_transcript_vanished() {
        // Boundary TWO: the seed was validated when the fork was
        // built, but the transcript can be deleted before the pane
        // launches. `--resume` on a dead id exits immediately with no
        // pane and no error, so the fork's gate would wait forever on
        // a reviewer that never started.
        let wt = std::path::Path::new("/repo/.clank/worktrees/x");
        let empty = tempfile::tempdir().unwrap();
        for tool in [Tool::Claude, Tool::Codex, Tool::Grok] {
            let desc = AgentDescription {
                tool,
                launch: None,
                initial_prompt: None,
            };
            let spec = crate::cli::fork::ForkSpec {
                tool,
                from_session: Some("gone-999".into()),
                prompt: "orient".into(),
            };
            let c = compose_fork_launch(&lbl("tester"), &spec, &desc, wt, Some(empty.path()));
            assert!(
                !c.args.iter().any(|a| a == "--resume" || a == "fork"),
                "{tool:?} must not resume a transcript that is gone: {:?}",
                c.args
            );
            // Still carries the orientation prompt — now behind the
            // session name for the tools whose title is derived.
            assert!(
                c.args.iter().any(|a| a.ends_with("orient")),
                "{tool:?} fresh launch still carries the orientation prompt: {:?}",
                c.args
            );
        }
    }

    #[test]
    fn fork_launch_still_resumes_when_the_store_cannot_be_probed() {
        // opencode's store is unprobeable, so the probe answers None
        // — "cannot tell", never "gone". Treating it as gone would
        // silently discard LIVE sessions, which is a worse bug than
        // the one being fixed.
        let desc = AgentDescription {
            tool: Tool::OpenCode,
            launch: None,
            initial_prompt: None,
        };
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::OpenCode,
            from_session: Some("ses_0af1b2c3d4e5f607".into()),
            prompt: "orient".into(),
        };
        let empty = tempfile::tempdir().unwrap();
        let c = compose_fork_launch(
            &lbl("tester"),
            &spec,
            &desc,
            std::path::Path::new("/repo/.clank/worktrees/x"),
            Some(empty.path()),
        );
        assert!(
            c.args.iter().any(|a| a == "--session"),
            "unprobeable must still resume: {:?}",
            c.args
        );
    }

    #[test]
    fn fork_launch_composes_per_tool_with_orientation_prompt() {
        // clank-fork-worktree-sessions: the seeded fork spec turns
        // into the per-tool fork argv with the orientation prompt
        // as the trailing positional. Both tools fork cleanly
        // (verified live 2026-06-10 — the stale "codex can't fork"
        // research claim is dead).
        let desc = AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        };
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Claude,
            from_session: Some("abc-123".into()),
            prompt: "You are `claude` in worktree `x`…".into(),
        };
        let wt = std::path::Path::new("/repo/.clank/worktrees/x");
        let home = home_with_session(Tool::Claude, "abc-123");
        let c = compose_fork_launch(&lbl("tester"), &spec, &desc, wt, Some(home.path()));
        assert_eq!(c.program, "claude");
        // claude takes cwd from the pane — no -C.
        assert_eq!(
            c.args,
            vec![
                "--resume",
                "abc-123",
                "--fork-session",
                // Named for the FORK, not the session it was copied from.
                "-n",
                "x · tester",
                "You are `claude` in worktree `x`…",
            ]
        );

        let desc = AgentDescription {
            tool: Tool::Codex,
            launch: None,
            initial_prompt: None,
        };
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Codex,
            from_session: Some("def-456".into()),
            prompt: "orient".into(),
        };
        let codex_home = home_with_session(Tool::Codex, "def-456");
        let c = compose_fork_launch(&lbl("tester"), &spec, &desc, wt, Some(codex_home.path()));
        assert_eq!(c.program, "codex");
        // codex gets -C <worktree> so it doesn't prompt for the cwd
        // (fork-codex-cd-flag).
        assert_eq!(
            c.args,
            vec![
                "fork",
                "-C",
                "/repo/.clank/worktrees/x",
                "def-456",
                // codex has no name flag: the name leads the prompt.
                "x · tester — orient",
            ]
        );
    }
}
