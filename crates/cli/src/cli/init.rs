//! `clank init` — scaffold `.clank/` in a repo + bootstrap the
//! calling agent's identity if running inside claude/codex.
//!
//! Two phases:
//! - **Phase 1 (always runs, non-interactive):** write
//!   `.clank/.gitignore`, write `.claude/settings.local.json`
//!   with claude edit-permission rules for `.clank/agents/**`,
//!   warn if the root gitignore swallows any `.clank/` path the
//!   plan needs tracked.
//! - **Phase 2 (only when running inside an agent):** prompt
//!   for the agent's label and whether it's the repo's master,
//!   then bind the session via `clank as` and (if master) write
//!   `.clank/config.json`. `--yes` skips prompts; falling back
//!   to tool-name + reviewers.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::{InitArgs, resolve_repo};
use crate::agent_env::detect_session_from_env;
use crate::agent_store::{
    agent_config_path, bind_session_to_agent, load_agent_config, load_all_agent_configs,
    save_agent_config,
};
use clank_core::ids::AgentLabel;
use clank_core::vocab::{Role, Tool};

use crate::init_facts::{
    CLANK_GITIGNORE_BODY as GITIGNORE_BODY,
    CLANK_GITIGNORE_LEGACY_BODIES as LEGACY_GITIGNORE_BODIES, CLAUDE_ALLOW_RULES,
    POST_REWRITE_BODY, POST_REWRITE_MARKER,
};

pub async fn run(args: InitArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    write_scaffold(&repo)?;
    write_claude_perms(&repo)?;
    write_post_rewrite_hook(&repo, args.force_hooks)?;
    warn_if_globally_excluded(&repo);
    let migrated_labels = migrate_legacy_agents_block_collecting(&repo)?;
    let default_agent_labels = seed_default_agents(&repo, &migrated_labels)?;
    bootstrap_agent_identity(&repo, args.yes, &default_agent_labels).await?;
    Ok(())
}

/// Migrate the legacy `.clank/config.json#/agents` block to
/// per-agent skeletons + sentinel, then remove the key.
///
/// Plan: `agents-declaration-is-user-local`. Two cases:
/// - `Some(non-empty vec)`: materialize each entry into
///   `.clank/agents/<label>/config.json`, MERGING with any
///   pre-existing skeleton state (preserve
///   session/auto_mode/wfw_timeout).
/// - `Some(vec![])`: write the explicit-empty sentinel
///   `.clank/agents/.empty` (codex 861c364 catch — preserves
///   the today-semantic that `agents: []` disables user-scope
///   fallback).
///
/// In BOTH cases, remove the `agents` key from
/// `.clank/config.json`. Idempotent: re-running with the key
/// already absent is a no-op.
pub(crate) fn migrate_legacy_agents_block(repo: &Path) -> anyhow::Result<()> {
    migrate_legacy_agents_block_collecting(repo).map(|_| ())
}

/// Same as [`migrate_legacy_agents_block`] but returns the set of
/// labels that were migrated from the legacy block in case A
/// (Some(non-empty)). Empty for case B (sentinel) and the no-op
/// case. Plan agents-declaration-is-user-local + codex 3cb8002:
/// the migrated set feeds bootstrap_agent_identity's
/// `preserve_existing_role` check — the user stated role for these
/// labels via the legacy block, just like user-scope
/// `default_agents`.
pub(crate) fn migrate_legacy_agents_block_collecting(
    repo: &Path,
) -> anyhow::Result<std::collections::HashSet<AgentLabel>> {
    let cfg_path = repo.join(".clank/config.json");
    let body = match std::fs::read_to_string(&cfg_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(std::collections::HashSet::new());
        }
        Err(e) => {
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("reading {}", cfg_path.display()));
        }
    };
    let mut value: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", cfg_path.display()))?;
    let obj = match value.as_object_mut() {
        Some(o) => o,
        None => return Ok(std::collections::HashSet::new()),
    };
    let agents_val = match obj.remove("agents") {
        Some(v) => v,
        None => return Ok(std::collections::HashSet::new()), // No legacy block; nothing to do.
    };
    let agents: Vec<crate::cli::config::DefaultAgent> = serde_json::from_value(agents_val)
        .with_context(|| format!("parsing legacy `agents` block in {}", cfg_path.display()))?;
    let mut migrated_labels = std::collections::HashSet::new();
    if agents.is_empty() {
        // Case B: explicit empty → quarantine any pre-existing
        // skeleton config.json files (their declarations were
        // suppressed by the legacy `agents: []` block; they must
        // STAY suppressed across the migration) and write the
        // sentinel. Codex 36624a5 catch: without the quarantine,
        // a stale `.clank/agents/<label>/config.json` left over
        // from before the `agents: []` block could be resurrected
        // by `clank agent add` (which clears the sentinel) or by
        // a subsequent operation that reads skeletons directly.
        //
        // Feedback subdirs are preserved (review history stays).
        // We delete the config.json only.
        let agents_dir = repo.join(".clank/agents");
        if agents_dir.is_dir() {
            for entry in std::fs::read_dir(&agents_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let cfg_path = entry.path().join("config.json");
                if cfg_path.is_file() {
                    std::fs::remove_file(&cfg_path).with_context(|| {
                        format!(
                            "removing stale skeleton {} during explicit-empty migration",
                            cfg_path.display()
                        )
                    })?;
                }
            }
        }
        let sentinel = repo.join(".clank/agents/.empty");
        if let Some(parent) = sentinel.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&sentinel, b"")?;
        println!(
            "migrated explicit-empty agents declaration to {} (now per-user; stale skeleton configs removed)",
            sentinel.strip_prefix(repo).unwrap_or(&sentinel).display()
        );
    } else {
        // Case A: materialize each entry into its skeleton.
        for entry in &agents {
            migrated_labels.insert(entry.label.clone());
            let skel_path = crate::agent_store::agent_config_path(repo, &entry.label);
            let existing = std::fs::read_to_string(&skel_path).ok();
            let mut cfg: clank_core::agent_config::AgentConfig = match existing {
                Some(body) => serde_json::from_str(&body)
                    .with_context(|| format!("parsing existing {}", skel_path.display()))?,
                None => clank_core::agent_config::AgentConfig::default(),
            };
            // Overwrite declaration fields with the legacy block's
            // values. Preserve session/auto_mode/wfw_timeout from
            // any pre-existing skeleton (per-machine state).
            cfg.role = entry.role;
            cfg.tool = entry.tool;
            cfg.launch = entry.launch.clone();
            cfg.initial_prompt = entry.initial_prompt.clone();
            crate::agent_store::save_agent_config(repo, &entry.label, &cfg)?;
        }
        println!(
            "migrated {} agents declaration(s) to per-agent skeletons (now per-user)",
            agents.len()
        );
    }
    // Write back the config without the agents key.
    let new_body = serde_json::to_string_pretty(&value)
        .with_context(|| format!("re-serializing {}", cfg_path.display()))?;
    std::fs::write(&cfg_path, new_body + "\n")?;
    Ok(migrated_labels)
}

/// Seed per-agent config skeletons from `~/.clank/config.json`'s
/// `default_agents` list, so a fresh repo has registered reviewers
/// the moment `clank init` returns. The all-reviewers gate requires
/// at least one registered reviewer to behave non-trivially; without
/// this step, a brand-new repo auto-approves every commit.
///
/// **Ordering**: must run before `bootstrap_agent_identity` so its
/// `has_existing_master` check at `:361` sees seeded master
/// entries (a seeded master flips the calling agent's role default
/// from master to reviewers).
///
/// **Idempotent**: any existing `.clank/agents/<label>/config.json`
/// is preserved (the user may have run `clank as` already,
/// populating the `session` field). Seeded skeletons set
/// `auto_mode: off` — the agent still has to run `clank auto on`
/// to actively respond. This is a *registration* helper, not an
/// *activation* helper; making it both was deferred to avoid
/// auto-enabling agents the user hasn't actively confirmed.
///
/// **Failure**: fails closed if the user config is malformed —
/// see `config::load_default_agents`.
///
/// Returns the set of labels declared in `default_agents` (regardless
/// of whether they were newly seeded or skipped because their config
/// already existed). `bootstrap_agent_identity` consumes this set to
/// decide which calling-agent roles to preserve.
fn seed_default_agents(
    repo: &Path,
    migrated_labels: &std::collections::HashSet<AgentLabel>,
) -> anyhow::Result<std::collections::HashSet<AgentLabel>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);

    // `declared` (returned) is the union of labels the user has
    // EXPLICITLY OPINED ABOUT across both scopes:
    // - User-scope `default_agents` (explicit user intent).
    // - Labels migrated from the legacy `agents` block at init time
    //   (the legacy block was an explicit user declaration of role).
    //
    // It does NOT include pre-existing per-agent skeletons that
    // came from a prior `clank as` bind, since those skeletons may
    // hold a default role rather than a user-stated preference.
    // bootstrap_agent_identity uses `declared` to preserve role
    // for labels the user has opined about — including those the
    // legacy block opined about — and to leave non-opined labels
    // eligible for the master-claim flow.
    let user_decls = crate::cli::config::load_default_agents(home.as_deref())?;
    let post_migration_skeletons = crate::cli::config::load_skeleton_agents(repo)?;
    let mut declared = std::collections::HashSet::new();
    for entry in &user_decls {
        declared.insert(entry.label.clone());
    }
    for label in migrated_labels {
        declared.insert(label.clone());
    }

    // Plan agents-declaration-is-user-local + codex 3cb8002 catch:
    // if the repo already has its own declaration (sentinel for
    // explicit-empty, OR any per-agent skeleton because the
    // legacy-block migration above just materialized them), do
    // NOT seed user-scope defaults — that would conflate the
    // repo's REPLACE override with the user's defaults.
    //
    // Without this check, init's ordering on a separate-HOME repo:
    //   1. migrate_legacy_agents_block: alice (legacy) →
    //      .clank/agents/alice/config.json. Key removed.
    //   2. seed_default_agents (this fn): sees no legacy block
    //      now, falls back to user-scope `default_agents`,
    //      seeds codex too. The repo now has both alice AND
    //      codex even though pre-init it had ONLY alice.
    let repo_has_declaration = crate::cli::config::empty_sentinel_path(repo).is_file()
        || !post_migration_skeletons.is_empty();
    if repo_has_declaration {
        return Ok(declared);
    }

    // No repo declaration → seed user-scope `default_agents` as
    // new skeletons.
    if user_decls.is_empty() {
        return Ok(declared);
    }
    let mut seeded = Vec::new();
    let mut skipped = Vec::new();
    for entry in &user_decls {
        let config_path = crate::agent_store::agent_config_path(repo, &entry.label);
        if config_path.exists() {
            skipped.push(entry.label.as_str().to_string());
            continue;
        }
        let cfg = clank_core::agent_config::AgentConfig {
            role: entry.role,
            ..Default::default()
        };
        crate::agent_store::save_agent_config(repo, &entry.label, &cfg)?;
        seeded.push(entry.label.as_str().to_string());
    }
    if !seeded.is_empty() {
        println!(
            "seeded {} agent skeleton(s): {}",
            seeded.len(),
            seeded.join(", ")
        );
    }
    if !skipped.is_empty() {
        println!(
            "skipped {} existing agent(s): {}",
            skipped.len(),
            skipped.join(", ")
        );
    }
    Ok(declared)
}

/// Install the `post-rewrite` git hook so feedback files
/// follow commits through rebases/amends. Resolves the real
/// hook path via `git rev-parse --git-path` so this works in
/// linked worktrees too (where `.git` is a file pointing at
/// the worktree's gitdir; hooks live in the main repo's
/// shared `.git/hooks/`). If a foreign hook exists, print a
/// warning and leave it alone unless `force` is set.
fn write_post_rewrite_hook(repo: &Path, force: bool) -> anyhow::Result<()> {
    let Some(hook_path) = resolve_hook_path(repo, "post-rewrite") else {
        // Not a real git repo (resolve failed); skip silently.
        return Ok(());
    };
    if let Some(parent) = hook_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("creating hooks dir `{}`: {e}", parent.display()))?;
    }
    match std::fs::read_to_string(&hook_path) {
        Ok(existing) if existing.contains(POST_REWRITE_MARKER) => {
            // Already ours; refresh body in case it changed.
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            println!("{} up to date", hook_path.display());
        }
        Ok(_) if force => {
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            println!("force-overwrote {}", hook_path.display());
        }
        Ok(_) => {
            eprintln!(
                "warning: {} already exists with non-clank content; \
                 leaving it alone. Pass `clank init --force-hooks` to overwrite, \
                 or chain `clank rewire --from-stdin` into it manually.",
                hook_path.display()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            println!("wrote {}", hook_path.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Resolve `<git-common-dir>/hooks/<name>` via
/// `git rev-parse --git-path hooks/<name>`. The output is
/// relative to the repo's working directory; we join to absolute
/// for clarity.
fn resolve_hook_path(repo: &Path, name: &str) -> Option<std::path::PathBuf> {
    let arg = format!("hooks/{name}");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-path", &arg])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let p = std::path::PathBuf::from(&raw);
    Some(if p.is_absolute() { p } else { repo.join(p) })
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn write_scaffold(repo: &Path) -> anyhow::Result<()> {
    let clank_dir = repo.join(".clank");
    let plans_dir = clank_dir.join("plans");
    std::fs::create_dir_all(&plans_dir)?;

    let gitignore = clank_dir.join(".gitignore");
    match std::fs::read_to_string(&gitignore) {
        Ok(existing) if existing == GITIGNORE_BODY => {
            println!("{} already up to date", gitignore.display());
        }
        Ok(existing) if LEGACY_GITIGNORE_BODIES.contains(&existing.as_str()) => {
            // Known prior content — silently upgrade.
            std::fs::write(&gitignore, GITIGNORE_BODY)?;
            println!("upgraded {}", gitignore.display());
        }
        Ok(_existing) => {
            anyhow::bail!(
                "{} exists with different content; refusing to overwrite. \
                 Inspect it, delete it, or edit it to match the documented content:\n{}",
                gitignore.display(),
                GITIGNORE_BODY
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&gitignore, GITIGNORE_BODY)?;
            println!("wrote {}", gitignore.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Warn (but don't fail) if some ancestor `.gitignore` or
/// `core.excludesFile` excludes paths that need to be tracked.
///
/// Probes every `.clank/` subpath the plan requires committed
/// (`plans/`, `finished/`, `config.json`, `agents/`) and parses
/// `git check-ignore -v`'s structured output:
///   `<source_file>:<line>:<pattern>\t<probed_path>`
/// We suppress only when `<source_file>` is the `.clank/.gitignore`
/// we just wrote. Substring matching on the whole record would be
/// fooled by paths or patterns that happen to contain that literal.
fn warn_if_globally_excluded(repo: &Path) {
    const TRACKED_PROBES: &[&str] = &[
        ".clank/plans",
        ".clank/finished",
        ".clank/config.json",
        ".clank/agents",
    ];
    for rel in TRACKED_PROBES {
        check_one(repo, rel);
    }
}

fn check_one(repo: &Path, rel: &str) {
    let probe = repo.join(rel);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-v"])
        .arg(probe.as_path())
        .output();
    let Ok(output) = output else { return };
    match output.status.code() {
        Some(0) => {}
        Some(1) => return,
        _ => {
            tracing::debug!(
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                rel,
                "check-ignore probe failed unexpectedly"
            );
            return;
        }
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("").trim_end();
    if line.is_empty() {
        return;
    }
    if matched_by_clank_gitignore(line) {
        return;
    }
    eprintln!(
        "warning: an ancestor .gitignore (or core.excludesFile) excludes {rel} — \
         tracked clank files would be hidden. Source:\n  {line}"
    );
}

/// Write/merge `.claude/settings.local.json` with the
/// `permissions.allow` rules that let claude edit
/// `.clank/agents/**` (the agent's own dir) without prompting on
/// every Write/Edit. Tag-merge: never clobber unrelated allow
/// entries the user may have added.
///
/// Codex's permission model is shell-command-based, not file-path-
/// based, so there's no equivalent file to write for codex —
/// its sandbox already permits writes inside the workspace.
fn write_claude_perms(repo: &Path) -> anyhow::Result<()> {
    let claude_dir = repo.join(".claude");
    let path = claude_dir.join("settings.local.json");

    let mut value: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("parsing `{}` as JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };

    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    let permissions = obj
        .entry("permissions".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let permissions = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`permissions` is not a JSON object"))?;
    let allow = permissions
        .entry("allow".to_string())
        .or_insert_with(|| serde_json::json!([]));
    let allow = allow
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("`permissions.allow` is not a JSON array"))?;

    let mut added = false;
    for rule in CLAUDE_ALLOW_RULES {
        let exists = allow.iter().any(|v| v.as_str() == Some(*rule));
        if !exists {
            allow.push(serde_json::Value::String((*rule).into()));
            added = true;
        }
    }

    std::fs::create_dir_all(&claude_dir)?;
    let serialized = serde_json::to_string_pretty(&value)?;
    std::fs::write(&path, format!("{serialized}\n"))?;
    if added {
        println!("wrote claude permissions to {}", path.display());
    } else {
        println!("{} already has clank permissions", path.display());
    }
    Ok(())
}

/// Phase 2: if running inside an agent, prompt for label +
/// master? and write the bootstrap config. Skipped silently if
/// no session env detected (caller's not in an agent).
async fn bootstrap_agent_identity(
    repo: &Path,
    yes: bool,
    default_agent_labels: &std::collections::HashSet<AgentLabel>,
) -> anyhow::Result<()> {
    let detected = match detect_session_from_env() {
        Ok(d) => d,
        Err(e) => {
            // Hostile env (both session vars set, garbage value).
            // Don't fail init; just skip the bootstrap.
            eprintln!("skipping agent bootstrap: {e}");
            return Ok(());
        }
    };
    let Some((tool, session_id)) = detected else {
        println!(
            "not running inside an agent (no CLAUDE_CODE_SESSION_ID / \
             CODEX_THREAD_ID). Skipping identity bootstrap — run \
             `clank init` (or `clank as <label>`) from inside your \
             agent to bind."
        );
        return Ok(());
    };

    // Codex 7e2983f catch: respect explicit-empty (`.empty`
    // sentinel) — don't write a hidden skeleton that a
    // subsequent `clank agent add` would resurrect by clearing
    // the sentinel.
    if crate::cli::config::empty_sentinel_path(repo).is_file() {
        println!(
            "explicit-empty agents declaration is active \
             (.clank/agents/.empty present); skipping identity \
             bootstrap. To register the calling agent, run \
             `clank agent add <label> --tool {}` (which will \
             clear the sentinel as the 0→1 transition).",
            tool.as_str()
        );
        let _ = session_id; // kept for symmetry; not bound here
        return Ok(());
    }

    let default_label = match tool {
        Tool::Claude => "claude",
        Tool::Codex => "codex",
    };

    let interactive = !yes && std::io::stdin().is_terminal();
    let label_raw = if interactive {
        prompt_with_default(&format!("Agent label [{default_label}]: "), default_label)?
    } else {
        default_label.to_string()
    };
    let label = AgentLabel::parse(&label_raw)
        .map_err(|e| anyhow::anyhow!("invalid label `{label_raw}`: {e}"))?;

    // If the calling agent's label is declared in the user's
    // `default_agents` list, the user has stated their preferred
    // role for this label. Phase 2's master-claim logic must
    // respect that — independent of whether the agent has been
    // bound by a prior `clank as` (session present) or is a fresh
    // seeded skeleton (session absent).
    //
    // The previous discriminator (`session.is_none()`) handled the
    // fresh-skeleton case but missed the bound case: codex declared
    // as reviewer + bound by prior `clank as` + no master → init
    // would have flipped codex to master, overriding the user's
    // declared default. Codex caught this; this fix scopes to
    // "label in default_agents" instead.
    let preserve_existing_role = default_agent_labels.contains(&label);

    let has_existing_master = load_all_agent_configs(repo)?
        .iter()
        .any(|(_, cfg)| cfg.role == Role::Master);

    let make_master = if preserve_existing_role {
        false
    } else if has_existing_master {
        if interactive {
            prompt_yes_no(
                "Default this agent to master role (vs reviewer)? [y/N] ",
                false,
            )?
        } else {
            false
        }
    } else if interactive {
        prompt_yes_no(
            "No master agent in this repo yet. Claim master? [Y/n] ",
            true,
        )?
    } else {
        true
    };

    // Bind via the shared helper — this preserves the
    // "one session, one label" invariant that `clank as` enforces,
    // so a sequence like `clank as alice; clank init --yes`
    // doesn't leave the session bound to BOTH alice and the
    // tool-name default. Stale bindings on other agents get
    // cleared atomically with the new bind.
    let outcome = bind_session_to_agent(repo, &label, tool, &session_id)?;
    println!(
        "bound {} session {} → agent `{}` ({})",
        tool.as_str(),
        session_id.as_str(),
        outcome.label.as_str(),
        agent_config_path(repo, &label).display(),
    );
    for other in outcome.cleared_from {
        println!("  (cleared stale binding on `{}`)", other.as_str());
    }

    if make_master {
        // Role is a per-user preference; write to the agent's
        // own config. No repo-shared file involved.
        let mut cfg = load_agent_config(repo, &label)?.unwrap_or_default();
        cfg.role = Role::Master;
        save_agent_config(repo, &label, &cfg)?;
        println!("  role: master");
    }
    Ok(())
}

fn prompt_with_default(prompt: &str, default: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_yes_no(prompt: &str, default: bool) -> anyhow::Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let trimmed = line.trim().to_ascii_lowercase();
    Ok(match trimmed.as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        "" => default,
        _ => default,
    })
}

/// True if the matched rule lives in `<repo>/.clank/.gitignore`
/// (the file we just wrote). `git check-ignore -v` emits records as
/// `<source_file>:<line>:<pattern>\t<probed>`; we parse the
/// `source_file` column and check it's our managed file.
fn matched_by_clank_gitignore(record: &str) -> bool {
    let (source_part, _probed) = match record.split_once('\t') {
        Some(parts) => parts,
        None => return false,
    };
    let source_file = match source_part.split(':').next() {
        Some(s) => s,
        None => return false,
    };
    let p = Path::new(source_file);
    p.file_name().is_some_and(|n| n == ".gitignore")
        && p.parent().is_some_and(|parent| parent.ends_with(".clank"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let s = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "--quiet", "--initial-branch=main"])
            .status()
            .unwrap();
        assert!(s.success());
        dir
    }

    #[test]
    fn writes_scaffold_creates_plans_dir_and_gitignore() {
        let dir = init_repo();
        write_scaffold(dir.path()).unwrap();
        assert!(dir.path().join(".clank/plans").is_dir());
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(body, GITIGNORE_BODY);
        // Sanity: anchored patterns blanket the per-agent subtree.
        assert!(body.contains("/agents/"));
        assert!(body.contains("/feedback/"));
        assert!(body.contains("/cache/"));
    }

    #[test]
    fn writes_scaffold_is_idempotent_when_content_matches() {
        let dir = init_repo();
        write_scaffold(dir.path()).unwrap();
        write_scaffold(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(body, GITIGNORE_BODY);
    }

    #[test]
    fn writes_scaffold_upgrades_legacy_body() {
        let dir = init_repo();
        let gitignore = dir.path().join(".clank/.gitignore");
        std::fs::create_dir_all(gitignore.parent().unwrap()).unwrap();
        // Seed with the legacy body that pre-dated task #265.
        std::fs::write(&gitignore, "feedback/\ncache/\n").unwrap();
        write_scaffold(dir.path()).unwrap();
        let body = std::fs::read_to_string(&gitignore).unwrap();
        assert_eq!(body, GITIGNORE_BODY);
    }

    #[test]
    fn matched_by_clank_gitignore_recognises_our_managed_file() {
        // Standard git check-ignore -v output: <source>:<line>:<pattern>\t<probed>.
        let record = ".clank/.gitignore:1:plans/extra\t.clank/plans/extra/foo";
        assert!(matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_ancestor_gitignore() {
        // Pattern column mentions the literal `.clank/.gitignore`
        // but the matching rule is in the repo-root .gitignore.
        let record = ".gitignore:5:!.clank/.gitignore\t.clank/.gitignore";
        assert!(!matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_unrelated_source() {
        let record = "../.gitignore:2:.clank/\t.clank/plans";
        assert!(!matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_missing_tab() {
        assert!(!matched_by_clank_gitignore(""));
        assert!(!matched_by_clank_gitignore(".clank/.gitignore:1:plans/"));
    }

    #[test]
    fn writes_claude_perms_creates_file_with_allow_rules() {
        let dir = init_repo();
        write_claude_perms(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let allow = v["permissions"]["allow"].as_array().unwrap();
        let entries: Vec<&str> = allow.iter().filter_map(|x| x.as_str()).collect();
        assert!(entries.contains(&"Write(.clank/agents/**)"));
        assert!(entries.contains(&"Edit(.clank/agents/**)"));
        assert!(entries.contains(&"Read(.clank/agents/**)"));
    }

    #[test]
    fn writes_claude_perms_preserves_existing_allow_entries() {
        let dir = init_repo();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.local.json"),
            r#"{"permissions":{"allow":["Bash(ls *)"]}}"#,
        )
        .unwrap();
        write_claude_perms(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let allow = v["permissions"]["allow"].as_array().unwrap();
        let entries: Vec<&str> = allow.iter().filter_map(|x| x.as_str()).collect();
        assert!(entries.contains(&"Bash(ls *)"), "existing entry dropped");
        assert!(entries.contains(&"Write(.clank/agents/**)"));
    }

    #[test]
    fn writes_claude_perms_is_idempotent() {
        let dir = init_repo();
        write_claude_perms(dir.path()).unwrap();
        write_claude_perms(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let allow = v["permissions"]["allow"].as_array().unwrap();
        // Three entries, no duplicates after re-run.
        let writes = allow
            .iter()
            .filter(|x| x.as_str() == Some("Write(.clank/agents/**)"))
            .count();
        assert_eq!(writes, 1, "duplicate entry on re-run");
    }

    /// `check-ignore` matrix per D10: with the recommended root
    /// gitignore + the managed `.clank/.gitignore`, which paths
    /// are tracked vs ignored.
    ///
    /// Check-ignore matrix for the simplified tracking model:
    /// only `plans/` and `finished/` need to be tracked;
    /// everything else under `.clank/` is per-user / local
    /// (agents/, feedback/, cache/, config.json — none of these
    /// exist anymore as repo-shared state).
    #[test]
    fn check_ignore_matrix_simplified() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join(".gitignore"),
            "/target/\n\
             .clank/*\n\
             !.clank/plans/\n\
             !.clank/finished/\n",
        )
        .unwrap();
        write_scaffold(dir.path()).unwrap();

        let probe = |rel: &str| -> bool {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["check-ignore", rel])
                .output()
                .unwrap();
            // exit 0 = ignored; 1 = tracked.
            out.status.code() == Some(0)
        };

        // Tracked.
        assert!(!probe(".clank/plans/foo.md"));
        assert!(!probe(".clank/finished/foo/seal.md"));
        // Per-user / local.
        assert!(probe(".clank/agents/alice/config.json"));
        assert!(probe(".clank/agents/alice/feedback/foo/abc.md"));
        assert!(probe(".clank/cache/anything"));
    }

    #[test]
    fn writes_post_rewrite_hook_when_absent() {
        let dir = init_repo();
        write_post_rewrite_hook(dir.path(), false).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".git/hooks/post-rewrite")).unwrap();
        assert!(body.contains(POST_REWRITE_MARKER));
        assert!(body.contains("clank rewire --from-stdin"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(".git/hooks/post-rewrite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "hook should be executable");
        }
    }

    #[test]
    fn writes_post_rewrite_hook_warns_on_foreign_content() {
        let dir = init_repo();
        let hook_path = dir.path().join(".git/hooks/post-rewrite");
        std::fs::write(&hook_path, "#!/bin/sh\necho not clank\n").unwrap();
        write_post_rewrite_hook(dir.path(), false).unwrap();
        let body = std::fs::read_to_string(&hook_path).unwrap();
        assert!(
            !body.contains(POST_REWRITE_MARKER),
            "foreign hook must NOT be overwritten without --force-hooks; body={body}"
        );
    }

    #[test]
    fn force_hooks_overwrites_foreign_content() {
        let dir = init_repo();
        let hook_path = dir.path().join(".git/hooks/post-rewrite");
        std::fs::write(&hook_path, "#!/bin/sh\necho not clank\n").unwrap();
        write_post_rewrite_hook(dir.path(), true).unwrap();
        let body = std::fs::read_to_string(&hook_path).unwrap();
        assert!(body.contains(POST_REWRITE_MARKER));
    }

    #[test]
    fn writes_post_rewrite_hook_idempotent_on_our_marker() {
        let dir = init_repo();
        write_post_rewrite_hook(dir.path(), false).unwrap();
        write_post_rewrite_hook(dir.path(), false).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".git/hooks/post-rewrite")).unwrap();
        assert!(body.contains(POST_REWRITE_MARKER));
        assert_eq!(body, POST_REWRITE_BODY);
    }

    #[test]
    fn writes_scaffold_refuses_on_drifted_content() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(dir.path().join(".clank/.gitignore"), "something/else\n").unwrap();
        let err = write_scaffold(dir.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("different content"), "unexpected error: {msg}");
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(
            body, "something/else\n",
            "drifted file must not be overwritten"
        );
    }
}
