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
use std::path::Path;

use anyhow::Context;

use super::{InitArgs, resolve_repo};
use crate::agent_env::detect_session_from_env;
use crate::agent_store::{
    agent_config_path, bind_session_to_agent, load_repo_config, save_repo_config,
};
use clank_core::ids::AgentLabel;
use clank_core::vocab::Tool;

/// The current canonical content of `.clank/.gitignore`.
///
/// `/feedback/` and `/cache/` use a leading slash so they're
/// anchored to `.clank/` — without it, `feedback/` would also
/// catch `.clank/agents/<n>/feedback/` (where peer reviews
/// live and MUST stay tracked).
const GITIGNORE_BODY: &str = "/feedback/\n/cache/\nagents/*/config.json\n";

/// Prior `.gitignore` bodies that should be silently upgraded to
/// `GITIGNORE_BODY`. Add an entry whenever this constant changes
/// so existing repos upgrade cleanly instead of hitting the
/// drift refusal.
const LEGACY_GITIGNORE_BODIES: &[&str] = &[
    "feedback/\ncache/\n",
    "feedback/\ncache/\nagents/*/config.json\n",
];

pub async fn run(args: InitArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    write_scaffold(&repo)?;
    write_claude_perms(&repo)?;
    warn_if_globally_excluded(&repo);
    bootstrap_agent_identity(&repo, args.yes).await?;
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
    const OUR_ALLOW: &[&str] = &[
        "Write(.clank/agents/**)",
        "Edit(.clank/agents/**)",
        "Read(.clank/agents/**)",
    ];

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
    for rule in OUR_ALLOW {
        let exists = allow.iter().any(|v| v.as_str() == Some(rule));
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
             CODEX_THREAD_ID). Skipping identity bootstrap — run \
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

    let make_master = if interactive {
        prompt_yes_no("Make this agent the master for this repo? [y/N] ", false)?
    } else {
        false
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
        let mut repo_cfg = load_repo_config(repo)?.unwrap_or_default();
        let prior = repo_cfg.master.clone();
        repo_cfg.master = Some(label.clone());
        save_repo_config(repo, &repo_cfg)?;
        match prior {
            Some(p) if p == label => println!("  (already master)"),
            Some(p) => println!(
                "  master: changed from `{}` to `{}` in .clank/config.json",
                p.as_str(),
                label.as_str()
            ),
            None => println!("  master: written to .clank/config.json"),
        }
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
        // Sanity: the body covers the right things.
        assert!(body.contains("feedback/"), "missing legacy feedback ignore");
        assert!(body.contains("cache/"), "missing cache ignore");
        assert!(
            body.contains("agents/*/config.json"),
            "missing per-agent config ignore"
        );
        assert!(
            !body.contains("agents/*/feedback"),
            "feedback dirs MUST stay tracked"
        );
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
    /// `.clank/*` only matches one level deep, so re-including
    /// `.clank/agents/` lets git RECURSE into the dir but doesn't
    /// automatically include children at depth 2+. The recursive
    /// `!.clank/agents/**` un-ignore is necessary; the targeted
    /// `.clank/agents/*/config.json` ignore must come AFTER it to
    /// actually apply.
    #[test]
    fn check_ignore_matrix_matches_d10_spec() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join(".gitignore"),
            "/target/\n\
             .clank/*\n\
             !.clank/plans/\n\
             !.clank/finished/\n\
             !.clank/config.json\n\
             !.clank/agents/\n\
             !.clank/agents/**\n\
             .clank/agents/*/config.json\n\
             .clank/cache/\n",
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

        assert!(
            !probe(".clank/config.json"),
            "config.json should be TRACKED"
        );
        assert!(
            !probe(".clank/agents/alice/feedback/foo/abc.md"),
            "agents/<n>/feedback/... should be TRACKED"
        );
        assert!(
            probe(".clank/agents/alice/config.json"),
            "agents/<n>/config.json should be IGNORED"
        );
        assert!(probe(".clank/cache/anything"), "cache/ should be IGNORED");
        assert!(!probe(".clank/plans/foo.md"), "plans/ should be TRACKED");
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
