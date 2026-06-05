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
use clank_core::vocab::{Role, Tool};
use std::collections::BTreeMap;

use super::{
    AgentAddArgs, AgentArgs, AgentCmd, AgentListArgs, AgentRemoveArgs, AgentSetRoleArgs,
    AgentStartArgs, resolve_repo,
};

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        AgentCmd::List(a) => list(a),
        AgentCmd::Start(a) => start(a),
        AgentCmd::Add(a) => add(a),
        AgentCmd::Remove(a) => remove(a),
        AgentCmd::SetRole(a) => set_role(a),
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

/// `clank agent start <name>`: exec into the agent's CLI tool
/// with session restored and the configured launch profile
/// applied. Requires a bound session (one policy: no fallback
/// to bare tool — see plan rationale at Phase B step 2 of
/// `agent-config-and-start`).
///
/// Launch profile source: the merged declaration's `launch`
/// field (per `agent-add-cli-and-repo-scope` — declaration is
/// the source of truth for role + tool + launch). Session
/// binding still comes from the per-agent skeleton.
fn start(args: AgentStartArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = AgentLabel::parse(&args.name)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.name))?;

    let cfg = match load_agent_config(&repo, &label)? {
        Some(c) => c,
        None => {
            anyhow::bail!(
                "no agent `{name}` in this repo. Create `.clank/agents/{name}/config.json` \
                 (or hand-edit one from the seed) before running `clank agent start`.",
                name = args.name
            );
        }
    };

    let session = cfg.session.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "agent `{name}` has no bound session. Run `clank as {name}` from inside the agent's CLI to bind.",
            name = args.name
        )
    })?;

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Propagate parse errors — silently falling back to an empty
    // declaration would drop the configured launch profile + leave
    // the user puzzled why `--skill X` wasn't applied. Codex caught
    // the unwrap_or_default on eef4c49.
    let merged = crate::cli::config::load_merged_agents(&repo, home.as_deref())?;
    let declaration_launch = merged
        .iter()
        .find(|e| e.label == label)
        .and_then(|e| e.launch.as_ref());

    let composed = compose_launch(&repo, session, declaration_launch);

    if args.print {
        print_composed(&composed);
        return Ok(());
    }

    exec_composed(composed)
}

/// Composed launch line: executable + argv + env additions.
#[derive(Debug, Clone)]
struct ComposedLaunch {
    program: String,
    args: Vec<String>,
    env_overrides: std::collections::BTreeMap<String, String>,
}

fn compose_launch(repo: &Path, session: &Session, launch: Option<&LaunchConfig>) -> ComposedLaunch {
    let tool = session.tool;
    let program = launch
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| tool.as_str().to_string());

    let launch_args = launch.map(|l| l.args.clone()).unwrap_or_default();

    let session_restore = session_restore_args(tool, session.id.as_str(), repo);

    let mut args = launch_args;
    args.extend(session_restore);

    let env_overrides = launch.map(|l| l.env.clone()).unwrap_or_default();

    ComposedLaunch {
        program,
        args,
        env_overrides,
    }
}

/// Tool-specific session-restore suffix. Goes AFTER `launch.args`
/// so codex's `resume` subcommand doesn't capture them (see Phase
/// B step 3 of the plan).
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
        file.default_agents = Some(agents);
        write_user_config(home_ref, &file)?;
        eprintln!("registered `{}` in user-scope `default_agents`", args.label);
    } else {
        // Repo-scope add: refuse if already in repo-scope. ALLOW
        // shadowing user-scope (REPLACE semantics) with stderr
        // notice.
        if in_repo {
            anyhow::bail!("agent `{}` already in repo-scope `agents`", args.label);
        }
        let mut current_repo = repo_set.unwrap_or_default();
        current_repo.push(entry);
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
        eprintln!("registered `{}` in repo-scope `agents`", args.label);
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

/// `clank agent set-role <label> <role> [--global]`.
fn set_role(args: AgentSetRoleArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let new_role: Role = args.role.into();
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
        let entry = agents
            .iter_mut()
            .find(|e| e.label == label)
            .ok_or_else(|| {
                anyhow::anyhow!("agent `{}` not in user-scope `default_agents`", args.label)
            })?;
        entry.role = new_role;
        file.default_agents = Some(agents);
        write_user_config(home_ref, &file)?;
        eprintln!(
            "set role of `{}` to `{}` in user-scope",
            args.label,
            new_role.as_str()
        );
    } else {
        let mut file = read_repo_config(&repo)?;
        let mut agents = file.agents.ok_or_else(|| {
            anyhow::anyhow!(
                "no repo-scope `agents` declaration; nothing to set-role in. Run `clank agent add` first."
            )
        })?;
        let entry = agents
            .iter_mut()
            .find(|e| e.label == label)
            .ok_or_else(|| anyhow::anyhow!("agent `{}` not in repo-scope `agents`", args.label))?;
        entry.role = new_role;
        file.agents = Some(agents);
        write_repo_config(&repo, &file)?;
        eprintln!(
            "set role of `{}` to `{}` in repo-scope",
            args.label,
            new_role.as_str()
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
