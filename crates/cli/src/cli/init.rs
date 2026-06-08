//! `clank init` — scaffold `.clank/` in a repo + bind the
//! calling agent's session if running inside claude/codex.
//!
//! Two phases:
//! - **Phase 1 (always runs, non-interactive):** write
//!   `.clank/.gitignore`, write `.claude/settings.local.json`
//!   with claude edit-permission rules for `.clank/agents/**`,
//!   warn if the root gitignore swallows any `.clank/` path the
//!   plan needs tracked, and (with `--team`) record the repo's
//!   team.
//! - **Phase 2 (only when running inside an agent):** prompt
//!   for the agent's label and bind the session via the shared
//!   bind helper. Identity (role/tool/launch) is team-based and
//!   lives in user/repo config, NOT in the per-agent skeleton —
//!   bootstrap only touches `session`.

use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::Context;

use super::{InitArgs, resolve_repo};
use crate::agent_env::detect_session_from_env;
use crate::agent_store::{agent_config_path, bind_session_to_agent};
use clank_core::ids::AgentLabel;
use clank_core::vocab::Tool;

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
    bootstrap_agent_identity(&repo, args.yes).await?;
    // When `--team <name>` is set, write the team field to
    // `<repo>/.clank/config.json` (new-schema). Validation:
    // the named team must exist in user-scope.
    if let Some(team_name) = args.team.as_deref() {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("$HOME not set; --team requires a user-scope config"))?;
        write_repo_team_field(home_ref, &repo, team_name)?;
    }
    Ok(())
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

/// Phase 2: if running inside an agent, prompt for the label and
/// bind the session. Skipped silently if no session env detected
/// (caller's not in an agent).
///
/// Identity (role/tool/launch) is team-based and lives in
/// user/repo config; bootstrap only writes the per-agent
/// skeleton's `session` field via the shared bind helper.
async fn bootstrap_agent_identity(repo: &Path, yes: bool) -> anyhow::Result<()> {
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
             CODEX_THREAD_ID). Skipping session bind — run \
             `clank init` (or `clank as <label>`) from inside your \
             agent to bind."
        );
        return Ok(());
    };

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

/// Plan: teams-based-agent-registration (phase 6a).
///
/// Write the `team: "<name>"` field to
/// `<repo>/.clank/config.json` using the new typed schema.
///
/// Validations:
/// - The named team must exist in user-scope
///   `~/.clank/config.json#/teams`. Reading nothing → error
///   (the user needs to `clank team create <name>` first).
/// - Existing repo config (legacy or new shape) round-trips
///   through the new schema's `extra` flatten catchall, so
///   unknown fields (review/hooks/diff sections, etc.) are
///   preserved.
fn write_repo_team_field(home: &Path, repo: &Path, team_name: &str) -> anyhow::Result<()> {
    use crate::cli::teams_config::{RepoConfigFile, TeamField, UserConfigFile};
    use anyhow::Context;
    use std::io::Write;

    // Validate against user-scope teams.
    let user_path = home.join(".clank/config.json");
    let user_cfg: UserConfigFile = match std::fs::read_to_string(&user_path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| {
            format!(
                "parsing {} as new-schema UserConfigFile",
                user_path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UserConfigFile::default(),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading {}", user_path.display())));
        }
    };
    if !user_cfg.teams.contains_key(team_name) {
        anyhow::bail!(
            "team `{team_name}` not declared in user-scope teams. Create it with \
             `clank team create {team_name}` (and optionally `clank team set-master \
             {team_name} <agent>` + `clank team add {team_name} <agent>`) first."
        );
    }

    let repo_cfg_path = repo.join(".clank/config.json");
    let mut repo_cfg: RepoConfigFile = match std::fs::read_to_string(&repo_cfg_path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| {
            format!(
                "parsing {} as new-schema RepoConfigFile",
                repo_cfg_path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RepoConfigFile::default(),
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context(format!("reading {}", repo_cfg_path.display()))
            );
        }
    };
    repo_cfg.team = Some(TeamField::Single(team_name.to_string()));

    // Atomic write via tempfile + rename.
    let parent = repo_cfg_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", repo_cfg_path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(&repo_cfg)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&repo_cfg_path).map_err(|e| e.error)?;
    eprintln!(
        "wrote `team: \"{team_name}\"` to {}",
        repo_cfg_path.display()
    );
    Ok(())
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

    // ── Plan: teams-based-agent-registration (phase 6a) ──

    fn write_user_teams(home: &Path, teams: &[&str]) {
        use crate::cli::teams_config::{TeamComposition, UserConfigFile};
        let mut cfg = UserConfigFile::default();
        for name in teams {
            cfg.teams
                .insert((*name).to_string(), TeamComposition::default());
        }
        let path = home.join(".clank/config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    }

    #[test]
    fn write_repo_team_field_records_team_in_new_schema() {
        use crate::cli::teams_config::{RepoConfigFile, TeamField};
        let user_home = tempfile::tempdir().unwrap();
        write_user_teams(user_home.path(), &["dev"]);
        let repo = init_repo();
        write_repo_team_field(user_home.path(), repo.path(), "dev").unwrap();
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        match parsed.team {
            Some(TeamField::Single(ref s)) => assert_eq!(s, "dev"),
            other => panic!("expected Single(dev); got {other:?}"),
        }
    }

    #[test]
    fn write_repo_team_field_rejects_unknown_team() {
        let user_home = tempfile::tempdir().unwrap();
        write_user_teams(user_home.path(), &["dev"]);
        let repo = init_repo();
        let err = write_repo_team_field(user_home.path(), repo.path(), "nonexistent").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not declared"));
        assert!(msg.contains("clank team create"));
        assert!(
            !repo.path().join(".clank/config.json").exists() || {
                let body = std::fs::read_to_string(repo.path().join(".clank/config.json"))
                    .unwrap_or_default();
                !body.contains("\"team\"")
            }
        );
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
