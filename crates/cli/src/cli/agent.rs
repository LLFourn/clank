//! `clank agent list` — enumerate registered agents in this repo
//! with role and bind state. Read-only; never mutates state.
//!
//! Source of truth: the **merged agent declaration** (repo-scope
//! `<repo>/.clank/config.json#/agents` if present, else user-scope
//! `~/.clank/config.json#/default_agents`). Per
//! `agent-add-cli-and-repo-scope`, the declaration drives role +
//! tool + launch; per-agent skeleton supplies session binding
//! state. List joins both. Declared agents without skeletons
//! render as unbound; orphan skeletons (skeleton without
//! declaration) are NOT listed — `clank doctor` surfaces them
//! separately.

use std::path::Path;

use anyhow::Context;
use serde::Serialize;

use crate::agent_store::{agent_config_path, load_agent_config, save_agent_config};
use crate::cli::config::{DefaultAgent, RepoConfigFile, UserConfigFile};
use clank_core::agent_config::{AgentConfig, LaunchConfig, Session};
use clank_core::ids::AgentLabel;
use clank_core::vocab::{AutoMode, Role, Tool};
use std::collections::BTreeMap;

use super::{
    AgentAddArgs, AgentArgs, AgentCmd, AgentListArgs, AgentPromoteArgs, AgentRemoveArgs,
    AgentStartArgs, resolve_repo,
};

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        AgentCmd::List(a) => list(a),
        AgentCmd::Start(a) => start(a),
        AgentCmd::Add(a) => add(a),
        AgentCmd::Remove(a) => remove(a),
        AgentCmd::Promote(a) => promote(a),
    }
}

#[derive(Debug, Serialize)]
struct AgentRow {
    label: String,
    role: String,
    bound: bool,
    tool: Option<String>,
    session_id: Option<String>,
}

impl AgentRow {
    fn from_join(
        label: &AgentLabel,
        role: Role,
        declared_tool: Option<Tool>,
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
            None => (false, declared_tool.map(|t| t.as_str().to_string()), None),
        };
        Self {
            label: label.as_str().to_string(),
            role: role.as_str().to_string(),
            bound,
            tool: tool_str,
            session_id,
        }
    }
}

fn list(args: AgentListArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Declaration is the source of truth for which agents are
    // registered; skeleton supplies session state (codex review of
    // eef4c49 — was scanning skeleton dirs).
    let declared = crate::cli::config::load_merged_agents(&repo, home.as_deref())?;
    let mut rows: Vec<AgentRow> = Vec::with_capacity(declared.len());
    for entry in &declared {
        let skeleton = load_agent_config(&repo, &entry.label)?;
        rows.push(AgentRow::from_join(
            &entry.label,
            entry.role,
            entry.tool,
            skeleton.as_ref().and_then(|c| c.session.as_ref()),
        ));
    }
    // Stable ordering: master first, then alphabetic by label.
    rows.sort_by(|a, b| {
        let role_key = |r: &str| match r {
            "master" => 0,
            _ => 1,
        };
        role_key(&a.role)
            .cmp(&role_key(&b.role))
            .then_with(|| a.label.cmp(&b.label))
    });
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

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let merged = crate::cli::config::load_merged_agents(&repo, home.as_deref())?;
    let entry = merged.iter().find(|e| e.label == label);

    let cfg = load_agent_config(&repo, &label)?;

    if entry.is_none() && cfg.is_none() {
        anyhow::bail!(
            "no agent `{name}` in this repo. Create `.clank/agents/{name}/config.json` \
             (or hand-edit one from the seed) before running `clank agent start`.",
            name = args.name
        );
    }

    let bound_session = cfg.as_ref().and_then(|c| c.session.as_ref());
    let composed = match (entry, bound_session) {
        (Some(entry), None) => compose_bootstrap_launch(entry)?,
        (None, None) => {
            // Declaration absent AND skeleton has no session: legacy-synth
            // path would have inserted `entry`, so this arm is the
            // "skeleton exists but synthesis returned tool=None and
            // launch.command absent" case. Surface the actionable hint.
            anyhow::bail!(no_bootstrap_tool_message(&label));
        }
        (entry_opt, Some(session)) => {
            let cfg = cfg.as_ref().expect("session implies skeleton");
            let declaration_launch = entry_opt.and_then(|e| e.launch.as_ref());
            let declaration_prompt = entry_opt.and_then(|e| e.initial_prompt.as_deref());
            let resolved_prompt = resolve_initial_prompt(declaration_prompt, cfg.auto_mode);
            compose_launch(
                &repo,
                session,
                declaration_launch,
                resolved_prompt.as_deref(),
            )
        }
    };

    if args.print {
        print_composed(&composed);
        return Ok(());
    }

    exec_composed(composed)
}

/// Bootstrap launch: declared agent has no bound session in this
/// repo. Spawn the bare tool with a seed prompt instructing the
/// agent to run `clank as <label>`, which creates the skeleton +
/// session binding. Subsequent `clank agent start <label>` calls
/// resume normally.
///
/// Plan: `agent-start-bootstraps-missing-skeleton`.
fn compose_bootstrap_launch(
    entry: &crate::cli::config::DefaultAgent,
) -> anyhow::Result<ComposedLaunch> {
    let program = entry
        .launch
        .as_ref()
        .and_then(|l| l.command.clone())
        .or_else(|| entry.tool.map(|t| t.as_str().to_string()))
        .ok_or_else(|| anyhow::anyhow!(no_bootstrap_tool_message(&entry.label)))?;

    let mut args = entry
        .launch
        .as_ref()
        .map(|l| l.args.clone())
        .unwrap_or_default();
    args.push(bootstrap_bind_prompt(&entry.label));

    let env_overrides = entry
        .launch
        .as_ref()
        .map(|l| l.env.clone())
        .unwrap_or_default();

    Ok(ComposedLaunch {
        program,
        args,
        env_overrides,
    })
}

/// Seed prompt for the bootstrap launch. Verbatim per the plan's
/// PINNED string — `bootstrap_uses_tool_from_declaration` test
/// asserts on this with `assert_eq!`, so a wording tweak fails the
/// test deliberately.
pub(super) fn bootstrap_bind_prompt(label: &AgentLabel) -> String {
    format!("Run `clank as {}` to bind this session.", label.as_str())
}

fn no_bootstrap_tool_message(label: &AgentLabel) -> String {
    let name = label.as_str();
    format!(
        "agent `{name}` has no bootstrap tool. The `tool` field lives \
         on the agents declaration, not the per-agent skeleton. \
         Either edit the `agents` entry in `.clank/config.json` (or \
         `default_agents` in `~/.clank/config.json`) to add \
         `\"tool\": \"claude\"` (or `\"codex\"`) under this label, \
         or remove and re-register: `clank agent remove {name} && \
         clank agent add {name} --tool <claude|codex>`."
    )
}

/// Composed launch line: executable + argv + env additions.
#[derive(Debug, Clone)]
struct ComposedLaunch {
    program: String,
    args: Vec<String>,
    env_overrides: std::collections::BTreeMap<String, String>,
}

/// Default `initial_prompt` when `auto_mode == On` and the
/// declaration's `initial_prompt` field is unset. Verbatim per
/// `agent-start-initial-prompt` Phase 3:
/// - Triggers turn-end with minimum surface (both tools produce
///   a one-line ack).
/// - Does NOT instruct the agent to run `clank wfw` itself (the
///   stop hook is the orchestrator; double-trigger to avoid).
/// - Does NOT expose orchestration internals (no "stop hook",
///   no "work loop") — agent only learns contextual location.
///
/// Test `resolve_initial_prompt_uses_default_when_auto_on_and_declaration_unset`
/// asserts on this string with `assert_eq!`, so a tweak fails
/// the test deliberately.
pub(super) const DEFAULT_AUTO_PROMPT: &str = "Session resumed.";

/// Resolve the initial prompt for an agent-start invocation.
///
/// Three-input policy:
/// - `declaration`: `DefaultAgent.initial_prompt`. `Some("")`
///   means "explicitly disable" — falls back to None, NOT
///   Some("") through. Lets a user with auto_mode=On opt out
///   of the prompt without disabling auto_mode itself.
/// - `auto_mode`: per-machine preference from the agent skeleton.
///   When On, supplies the [`DEFAULT_AUTO_PROMPT`] fallback.
/// - Returns the resolved prompt or None.
pub(super) fn resolve_initial_prompt(
    declaration: Option<&str>,
    auto_mode: AutoMode,
) -> Option<String> {
    if let Some(s) = declaration {
        if s.is_empty() {
            return None;
        }
        return Some(s.to_string());
    }
    if auto_mode == AutoMode::On {
        return Some(DEFAULT_AUTO_PROMPT.to_string());
    }
    None
}

fn compose_launch(
    repo: &Path,
    session: &Session,
    launch: Option<&LaunchConfig>,
    initial_prompt: Option<&str>,
) -> ComposedLaunch {
    let tool = session.tool;
    let program = launch
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| tool.as_str().to_string());

    let launch_args = launch.map(|l| l.args.clone()).unwrap_or_default();

    let session_restore = session_restore_args(tool, session.id.as_str(), repo);

    let mut args = launch_args;
    args.extend(session_restore);
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_string());
    }

    let env_overrides = launch.map(|l| l.env.clone()).unwrap_or_default();

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

fn shell_quote(s: &str) -> String {
    // POSIX single-quote escaping. Closes out for embedded
    // single quotes via the standard `'\''` dance.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(unix)]
fn exec_composed(c: ComposedLaunch) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&c.program);
    cmd.args(&c.args);
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
    let status = std::process::Command::new(&c.program)
        .args(&c.args)
        .envs(c.env_overrides.iter())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn `{}`: {e}", c.program))?;
    if !status.success() {
        anyhow::bail!("`{}` exited with status {}", c.program, status);
    }
    Ok(())
}

// ── Phase 4 + 5 + 6: clank agent add/remove/set-role ─────────────
//
// All three mutate the merged-declaration source of truth:
// repo-scope `<repo>/.clank/config.json` `agents` (default) or
// user-scope `~/.clank/config.json` `default_agents` (`--global`).
// The per-agent skeleton at `<repo>/.clank/agents/<label>/config.json`
// holds only per-machine state — these commands NEVER write role or
// launch into the skeleton. See Phase 5 of agent-add-cli-and-repo-scope.

/// `clank agent add <label> [...]`.
fn add(args: AgentAddArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let role: Role = args.role.into();
    let tool: Tool = args.tool.into();

    let repo = resolve_repo(args.repo.as_deref())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

    let env = parse_env_overrides(&args.launch_envs)?;
    let launch = build_launch(args.launch_cmd, args.launch_args, env);

    let entry = DefaultAgent {
        label: label.clone(),
        role,
        tool: Some(tool),
        launch,
        initial_prompt: args.initial_prompt.clone(),
    };

    // Pre-checks (in-memory, no writes yet). Use declaration-only
    // loaders — legacy skeleton fallback would conflate
    // pre-existing skeletons with explicit registrations and
    // refuse adds the user actually wants. Codex review of da71c84
    // drove the explicit declared-vs-merged split.
    let repo_set = crate::cli::config::load_repo_agents(&repo)?;
    let user_set = crate::cli::config::load_default_agents(home.as_deref())?;
    let in_repo = repo_set
        .as_ref()
        .map(|set| set.iter().any(|e| e.label == label))
        .unwrap_or(false);
    let in_user = user_set.iter().any(|e| e.label == label);

    if args.global {
        // User-scope add: refuse if label exists in user-scope OR
        // in repo-scope (cross-scope ambiguity).
        if in_user {
            anyhow::bail!(
                "agent `{}` already in user-scope `default_agents`",
                args.label
            );
        }
        if in_repo {
            anyhow::bail!(
                "agent `{}` is registered in repo-scope `agents` for this repo; \
                 adding it to user-scope would produce ambiguous merge behavior. \
                 Remove the repo-scope entry first or pick a different label.",
                args.label
            );
        }
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        let mut file = read_user_config(home_ref)?;
        let mut agents = file.default_agents.unwrap_or_default();
        agents.push(entry);
        // Plan: agent-promote-replaces-set-role — `add --role
        // master` enforces the same unique-master invariant as
        // `promote`. Reuses the shared helper so both paths
        // centralize the check.
        let demoted = if role == Role::Master {
            ensure_unique_master(&mut agents, &label)
        } else {
            Vec::new()
        };
        file.default_agents = Some(agents);
        write_user_config(home_ref, &file)?;
        eprintln!(
            "registered `{}` in user-scope `default_agents`{}",
            args.label,
            format_demoted_suffix(&demoted)
        );
    } else {
        // Repo-scope add: refuse if already in repo-scope. ALLOW
        // shadowing user-scope (REPLACE semantics) with stderr
        // notice.
        if in_repo {
            anyhow::bail!("agent `{}` already in repo-scope `agents`", args.label);
        }
        let mut current_repo = repo_set.unwrap_or_default();
        current_repo.push(entry);
        let demoted = if role == Role::Master {
            ensure_unique_master(&mut current_repo, &label)
        } else {
            Vec::new()
        };
        // Skeleton write FIRST (idempotent at-rest; preserves
        // existing per-machine state if a prior `clank as` bound
        // a session). Then declaration write. See plan Phase 5.
        write_skeleton_preserving_machine_state(&repo, &label)?;
        let mut file = read_repo_config(&repo)?;
        file.agents = Some(current_repo);
        write_repo_config(&repo, &file)?;
        if in_user {
            eprintln!(
                "note: repo-scope `{}` shadows user-scope default",
                args.label
            );
        }
        eprintln!(
            "registered `{}` in repo-scope `agents`{}",
            args.label,
            format_demoted_suffix(&demoted)
        );
    }
    Ok(())
}

/// `clank agent remove <label> [--global]`.
fn remove(args: AgentRemoveArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let repo = resolve_repo(args.repo.as_deref())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

    if args.global {
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        let mut file = read_user_config(home_ref)?;
        let mut agents = file.default_agents.ok_or_else(|| {
            anyhow::anyhow!("agent `{}` not in user-scope `default_agents`", args.label)
        })?;
        let before = agents.len();
        agents.retain(|e| e.label != label);
        if agents.len() == before {
            anyhow::bail!("agent `{}` not in user-scope `default_agents`", args.label);
        }
        // Empty list collapses to `None` so the key drops out of
        // the JSON (Option's skip_serializing_if).
        file.default_agents = if agents.is_empty() {
            None
        } else {
            Some(agents)
        };
        write_user_config(home_ref, &file)?;
        eprintln!("removed `{}` from user-scope `default_agents`", args.label);
    } else {
        let mut file = read_repo_config(&repo)?;
        let mut current = file
            .agents
            .clone()
            .ok_or_else(|| anyhow::anyhow!("agent `{}` not in repo-scope `agents`", args.label))?;
        let before = current.len();
        current.retain(|e| e.label != label);
        if current.len() == before {
            anyhow::bail!("agent `{}` not in repo-scope `agents`", args.label);
        }
        // Preserve the empty-list override semantic: if the user
        // explicitly registered `agents: []` AND removed the
        // last entry from a previously-populated list, keep
        // `agents: []` (empty override). Distinguish "we removed
        // the last entry from a list of N" (keep []) from "the
        // list was already empty" (covered above by the
        // bail!-on-no-change).
        file.agents = Some(current);
        write_repo_config(&repo, &file)?;
        eprintln!(
            "removed `{}` from repo-scope `agents` (per-agent dir + feedback preserved)",
            args.label
        );
    }
    Ok(())
}

/// Set `new_master`'s role to `Master` and demote every OTHER
/// entry with `Role::Master` to `Reviewer`. Returns the labels
/// that were demoted (caller uses these for the diagnostic).
///
/// One sweep over the slice. No I/O. This is the single
/// enforcement point for the "exactly one master per scope"
/// invariant — `promote` and `add --role master` both go through
/// it (plan: `agent-promote-replaces-set-role`).
pub(super) fn ensure_unique_master(
    agents: &mut [DefaultAgent],
    new_master: &AgentLabel,
) -> Vec<AgentLabel> {
    let mut demoted = Vec::new();
    for entry in agents.iter_mut() {
        if entry.label == *new_master {
            entry.role = Role::Master;
        } else if entry.role == Role::Master {
            entry.role = Role::Reviewer;
            demoted.push(entry.label.clone());
        }
    }
    demoted
}

fn format_demoted_suffix(demoted: &[AgentLabel]) -> String {
    if demoted.is_empty() {
        String::new()
    } else {
        let joined = demoted
            .iter()
            .map(|l| format!("`{}`", l.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" (demoted {joined})")
    }
}

/// `clank agent promote <label> [--global]`.
fn promote(args: AgentPromoteArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let repo = resolve_repo(args.repo.as_deref())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

    if args.global {
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        let mut file = read_user_config(home_ref)?;
        let mut agents = file.default_agents.ok_or_else(|| {
            anyhow::anyhow!("agent `{}` not in user-scope `default_agents`", args.label)
        })?;
        if !agents.iter().any(|e| e.label == label) {
            anyhow::bail!("agent `{}` not in user-scope `default_agents`", args.label);
        }
        // Detect no-op BEFORE mutating: target is already the only
        // master in this scope.
        let already_only_master = agents
            .iter()
            .all(|e| (e.label == label) == (e.role == Role::Master));
        if already_only_master {
            eprintln!(
                "note: `{}` is already the only master in user-scope",
                args.label
            );
            return Ok(());
        }
        let demoted = ensure_unique_master(&mut agents, &label);
        file.default_agents = Some(agents);
        write_user_config(home_ref, &file)?;
        eprintln!(
            "promoted `{}` to master in user-scope{}",
            args.label,
            format_demoted_suffix(&demoted)
        );
    } else {
        let mut file = read_repo_config(&repo)?;
        let mut agents = file.agents.ok_or_else(|| {
            anyhow::anyhow!(
                "no repo-scope `agents` declaration; nothing to promote in. Run `clank agent add` first."
            )
        })?;
        if !agents.iter().any(|e| e.label == label) {
            anyhow::bail!("agent `{}` not in repo-scope `agents`", args.label);
        }
        let already_only_master = agents
            .iter()
            .all(|e| (e.label == label) == (e.role == Role::Master));
        if already_only_master {
            eprintln!(
                "note: `{}` is already the only master in repo-scope",
                args.label
            );
            return Ok(());
        }
        let demoted = ensure_unique_master(&mut agents, &label);
        file.agents = Some(agents);
        write_repo_config(&repo, &file)?;
        eprintln!(
            "promoted `{}` to master in repo-scope{}",
            args.label,
            format_demoted_suffix(&demoted)
        );
    }
    Ok(())
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
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
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

/// Read `<repo>/.clank/config.json` as a typed [`RepoConfigFile`].
fn read_repo_config(repo: &Path) -> anyhow::Result<RepoConfigFile> {
    let path = repo.join(".clank/config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RepoConfigFile::default()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

fn write_repo_config(repo: &Path, file: &RepoConfigFile) -> anyhow::Result<()> {
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

/// Idempotent skeleton write: creates `<repo>/.clank/agents/<label>/config.json`
/// with DEFAULT per-machine state IF the file doesn't exist.
/// Pre-existing skeletons (e.g., from a prior `clank as <label>`
/// binding) are left untouched — `clank agent add` MUST NOT clobber
/// session / auto_mode / wfw_timeout.
fn write_skeleton_preserving_machine_state(repo: &Path, label: &AgentLabel) -> anyhow::Result<()> {
    let path = agent_config_path(repo, label);
    if path.exists() {
        return Ok(()); // preserve existing per-machine state
    }
    save_agent_config(repo, label, &AgentConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
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

    #[test]
    fn compose_launch_appends_initial_prompt_when_set() {
        let s = claude_session();
        let c = compose_launch(Path::new("/repo"), &s, None, Some("custom prompt"));
        assert_eq!(
            c.args.last().map(|s| s.as_str()),
            Some("custom prompt"),
            "trailing arg should be the prompt; got: {:?}",
            c.args
        );
    }

    #[test]
    fn compose_launch_omits_prompt_when_none() {
        let s = claude_session();
        let c = compose_launch(Path::new("/repo"), &s, None, None);
        // Current behavior preserved exactly: trailing arg is the session id, no prompt.
        assert_eq!(
            c.args.last().map(|s| s.as_str()),
            Some("aaaaaaaa-1111-2222-3333-444444444444"),
            "no trailing prompt should be appended; got: {:?}",
            c.args
        );
    }

    #[test]
    fn compose_launch_codex_prompt_is_final_positional_after_cd() {
        // For codex, --cd <repo> comes from session_restore_args
        // BEFORE the prompt. The prompt is the final positional.
        let s = codex_session();
        let c = compose_launch(Path::new("/repo"), &s, None, Some("ack"));
        // Expected: ["resume", "<id>", "--cd", "/repo", "ack"]
        assert_eq!(c.args.last().map(|s| s.as_str()), Some("ack"));
        // --cd <repo> appears before the prompt.
        let cd_pos = c.args.iter().position(|s| s == "--cd").unwrap();
        let prompt_pos = c.args.iter().position(|s| s == "ack").unwrap();
        assert!(
            cd_pos < prompt_pos,
            "--cd must come before prompt; argv: {:?}",
            c.args
        );
    }

    // ── resolve_initial_prompt policy tests ────────────────────

    #[test]
    fn resolve_initial_prompt_uses_declaration_field_when_set() {
        let out = resolve_initial_prompt(Some("foo"), AutoMode::Off);
        assert_eq!(out, Some("foo".to_string()));
    }

    #[test]
    fn resolve_initial_prompt_uses_default_when_auto_on_and_declaration_unset() {
        // Pinned: exact equality against the constant so a future
        // tweak to DEFAULT_AUTO_PROMPT fails the test deliberately.
        let out = resolve_initial_prompt(None, AutoMode::On);
        assert_eq!(out, Some("Session resumed.".to_string()));
    }

    #[test]
    fn resolve_initial_prompt_returns_none_when_auto_off_and_declaration_unset() {
        let out = resolve_initial_prompt(None, AutoMode::Off);
        assert_eq!(out, None);
    }

    #[test]
    fn resolve_initial_prompt_declaration_wins_over_auto_default() {
        let out = resolve_initial_prompt(Some("custom"), AutoMode::On);
        assert_eq!(out, Some("custom".to_string()));
    }

    #[test]
    fn resolve_initial_prompt_empty_declaration_string_disables_prompt() {
        // Ruthless 0fe1567 pin: Some("") is the explicit-disable
        // escape hatch. Without this, the only way to opt out of
        // the auto_mode default would be to disable auto_mode
        // itself — coupling two unrelated concerns.
        let out = resolve_initial_prompt(Some(""), AutoMode::On);
        assert_eq!(out, None);
    }

    // ── compose_bootstrap_launch policy tests ─────────────────
    // Plan: agent-start-bootstraps-missing-skeleton.

    fn entry_with(
        tool: Option<Tool>,
        launch: Option<LaunchConfig>,
    ) -> crate::cli::config::DefaultAgent {
        crate::cli::config::DefaultAgent {
            label: label("phantom"),
            role: clank_core::vocab::Role::Reviewer,
            tool,
            launch,
            initial_prompt: None,
        }
    }

    #[test]
    fn bootstrap_uses_tool_from_declaration() {
        let entry = entry_with(
            Some(Tool::Claude),
            Some(LaunchConfig {
                command: None,
                args: vec!["--skill".into(), "ruthless".into()],
                env: Default::default(),
            }),
        );
        let composed = compose_bootstrap_launch(&entry).expect("compose");
        assert_eq!(composed.program, "claude");
        assert_eq!(
            composed.args,
            vec![
                "--skill".to_string(),
                "ruthless".to_string(),
                "Run `clank as phantom` to bind this session.".to_string(),
            ]
        );
    }

    #[test]
    fn bootstrap_prefers_launch_command_over_tool() {
        let entry = entry_with(
            Some(Tool::Claude),
            Some(LaunchConfig {
                command: Some("my-claude-wrapper".into()),
                args: vec![],
                env: Default::default(),
            }),
        );
        let composed = compose_bootstrap_launch(&entry).expect("compose");
        // launch.command wins; tool=claude is only the fallback.
        assert_eq!(composed.program, "my-claude-wrapper");
    }

    #[test]
    fn bootstrap_errors_when_no_tool_or_command() {
        let entry = entry_with(None, None);
        let err = compose_bootstrap_launch(&entry).expect_err("must error");
        let msg = format!("{err}");
        assert!(msg.contains("phantom"), "error names the label: {msg}");
        assert!(
            msg.contains("tool") && (msg.contains("claude") || msg.contains("codex")),
            "error mentions tool fix: {msg}"
        );
        // Codex 9c431de catch: the hint must point at the
        // DECLARATION (`.clank/config.json` agents block or user-
        // scope `default_agents`), NOT the per-agent skeleton at
        // `.clank/agents/<label>/config.json` (which has no `tool`
        // field). Pinned so a regression to "edit the skeleton"
        // fails the test deliberately.
        assert!(
            msg.contains(".clank/config.json"),
            "error must reference the repo-scope declaration file path; got: {msg}"
        );
        assert!(
            !msg.contains(".clank/agents/"),
            "error must NOT direct users to the per-agent skeleton path (no tool field there); got: {msg}"
        );
        assert!(
            msg.contains("clank agent remove") && msg.contains("clank agent add"),
            "error gives the remove-and-re-add path; got: {msg}"
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
        let out = resolve_initial_prompt(Some(""), AutoMode::Off);
        assert_eq!(out, None);
    }

    #[test]
    fn agent_row_bound_uses_session_tool() {
        let session = make_session();
        let row = AgentRow::from_join(
            &label("alice"),
            Role::Reviewer,
            Some(Tool::Codex), // declared tool (overridden by session)
            Some(&session),
        );
        assert_eq!(row.label, "alice");
        assert_eq!(row.role, "reviewer");
        assert!(row.bound);
        assert_eq!(row.tool.as_deref(), Some("claude")); // from session
        assert!(row.session_id.is_some());
    }

    #[test]
    fn agent_row_unbound_uses_declared_tool() {
        let row = AgentRow::from_join(&label("alice"), Role::Reviewer, Some(Tool::Claude), None);
        assert!(!row.bound);
        assert_eq!(row.tool.as_deref(), Some("claude"));
        assert!(row.session_id.is_none());
    }

    #[test]
    fn agent_row_unbound_no_declared_tool_shows_none() {
        let row = AgentRow::from_join(&label("alice"), Role::Reviewer, None, None);
        assert!(!row.bound);
        assert!(row.tool.is_none());
    }
}
