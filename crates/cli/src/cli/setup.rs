//! `clank setup` — install user-scope clank assets into
//! `~/.claude/` and `~/.codex/`.
//!
//! Writes three skill/command files (refuse-if-drifted; D8) and
//! tag-merges a `Stop` hook entry into each agent's user-wide
//! hook config (idempotent by command-prefix match; D8).
//!
//! Per the plan (D1) clank ships as a CLI that owns these
//! files — no separate plugin distribution. Re-running `clank
//! setup` after upgrading the binary refreshes everything.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::SetupArgs;

const CLAUDE_SKILL_BODY: &str = include_str!("setup_assets/claude_skill.md");
const CODEX_SKILL_BODY: &str = include_str!("setup_assets/codex_skill.md");
const CODEX_COMMAND_BODY: &str = include_str!("setup_assets/codex_command.md");

/// Match string used to identify "our" Stop hook entry inside a
/// user's settings.json / hooks.json. Anything whose `command`
/// starts with this is replaced on re-setup; everything else is
/// preserved. (D8: tag-merge by marker, not append-only.)
const HOOK_COMMAND_MARKER: &str = "clank stop-hook";

/// Per-tool hook timeout we write into the agent's hook config.
/// 24 hours — effectively infinite. The clank-side `wfw_timeout`
/// in the agent's local AgentConfig is the real timer; this just
/// stops the agent's hook runner from killing the process early.
const HOOK_TIMEOUT_SECS: u64 = 86400;

pub async fn run(args: SetupArgs) -> anyhow::Result<()> {
    let home = home_dir()?;
    let mut summary = Vec::<String>::new();

    install_skill(
        &home.join(".claude/skills/clank/SKILL.md"),
        CLAUDE_SKILL_BODY,
        args.force,
        args.dry_run,
        &mut summary,
    )?;
    install_skill(
        &home.join(".codex/skills/clank/SKILL.md"),
        CODEX_SKILL_BODY,
        args.force,
        args.dry_run,
        &mut summary,
    )?;
    install_skill(
        &home.join(".codex/commands/clank.md"),
        CODEX_COMMAND_BODY,
        args.force,
        args.dry_run,
        &mut summary,
    )?;

    merge_hook_into_settings(
        &home.join(".claude/settings.json"),
        ClaudeHook,
        args.dry_run,
        &mut summary,
    )?;
    merge_hook_into_settings(
        &home.join(".codex/hooks.json"),
        CodexHook,
        args.dry_run,
        &mut summary,
    )?;

    if summary.is_empty() {
        println!("clank setup: nothing to do (all assets already in place)");
    } else {
        for line in summary {
            println!("{line}");
        }
    }
    if args.dry_run {
        println!("(dry-run — no changes written)");
    }
    Ok(())
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME env var is unset; cannot locate user config dirs"))
}

/// Write a skill/command file the binary owns outright. Behavior:
/// - missing → write
/// - matches → no-op (record up-to-date)
/// - drifted → refuse unless `--force`
fn install_skill(
    path: &Path,
    expected: &str,
    force: bool,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    match std::fs::read_to_string(path) {
        Ok(existing) if existing == expected => {
            summary.push(format!("  ok    {}", path.display()));
        }
        Ok(_) if force => {
            if !dry_run {
                std::fs::write(path, expected)
                    .with_context(|| format!("writing `{}`", path.display()))?;
            }
            summary.push(format!("  force {}", path.display()));
        }
        Ok(_) => {
            anyhow::bail!(
                "{} exists with different content; pass --force to overwrite.",
                path.display()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !dry_run {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating `{}`", parent.display()))?;
                }
                std::fs::write(path, expected)
                    .with_context(|| format!("writing `{}`", path.display()))?;
            }
            summary.push(format!("  write {}", path.display()));
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Tool-specific knowledge for the hook merger.
trait HookKind {
    /// What goes into the `command` field of the JSON entry.
    fn command(&self) -> &'static str;
    /// Tool name for the diagnostic summary.
    fn tool_name(&self) -> &'static str;
    /// Whether this tool's hook entry supports `statusMessage`
    /// (visible while the hook runs).
    fn status_message(&self) -> Option<&'static str> {
        None
    }
}

struct ClaudeHook;
impl HookKind for ClaudeHook {
    fn command(&self) -> &'static str {
        "clank stop-hook --tool claude"
    }
    fn tool_name(&self) -> &'static str {
        "claude"
    }
    // Claude doesn't show statusMessage; skip.
}

struct CodexHook;
impl HookKind for CodexHook {
    fn command(&self) -> &'static str {
        "clank stop-hook --tool codex"
    }
    fn tool_name(&self) -> &'static str {
        "codex"
    }
    fn status_message(&self) -> Option<&'static str> {
        Some("Clank: checking for pending review work")
    }
}

/// Tag-merge our Stop hook entry into the user's hook-config
/// JSON. Replaces any existing entry whose `command` begins with
/// `HOOK_COMMAND_MARKER` (so re-installing after an upgrade
/// updates in place); preserves every unrelated Stop hook.
fn merge_hook_into_settings(
    path: &Path,
    kind: impl HookKind,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut value: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("parsing `{}` as JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };

    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    let hooks = obj
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` is not a JSON object"))?;
    let stop = hooks
        .entry("Stop".to_string())
        .or_insert_with(|| serde_json::json!([]));
    let stop = stop
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks.Stop` is not a JSON array"))?;

    // Remove any prior clank entries. The outer "matcher" wrapper
    // contains a nested `hooks` array; an entry is "ours" if ANY
    // inner hook has a command starting with the marker.
    let before = stop.len();
    stop.retain(|wrapper| !wrapper_is_clank(wrapper));
    let removed = before - stop.len();

    // Add our fresh entry.
    let mut inner_hook = serde_json::json!({
        "type": "command",
        "command": kind.command(),
        "timeout": HOOK_TIMEOUT_SECS,
    });
    if let Some(msg) = kind.status_message() {
        inner_hook.as_object_mut().unwrap().insert(
            "statusMessage".to_string(),
            serde_json::Value::String(msg.into()),
        );
    }
    stop.push(serde_json::json!({
        "hooks": [inner_hook],
    }));

    if !dry_run {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating `{}`", parent.display()))?;
        }
        let serialized = serde_json::to_string_pretty(&value)?;
        std::fs::write(path, format!("{serialized}\n"))
            .with_context(|| format!("writing `{}`", path.display()))?;
    }

    let action = if removed > 0 { "merge" } else { "add  " };
    summary.push(format!(
        "  {action} {path} ({tool} Stop hook)",
        path = path.display(),
        tool = kind.tool_name(),
    ));
    Ok(())
}

fn wrapper_is_clank(wrapper: &serde_json::Value) -> bool {
    let Some(inner) = wrapper.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    inner
        .iter()
        .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
        .any(|cmd| cmd.starts_with(HOOK_COMMAND_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn install_skill_writes_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/SKILL.md");
        let mut summary = Vec::new();
        install_skill(&path, "hello", false, false, &mut summary).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(summary[0].contains("write"));
    }

    #[test]
    fn install_skill_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "hello");
        let mut summary = Vec::new();
        install_skill(&path, "hello", false, false, &mut summary).unwrap();
        assert!(summary[0].contains("ok"));
    }

    #[test]
    fn install_skill_refuses_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "edited by user\n");
        let mut summary = Vec::new();
        let err = install_skill(&path, "hello", false, false, &mut summary).unwrap_err();
        assert!(err.to_string().contains("--force"), "unexpected: {err}");
        // File preserved.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited by user\n");
    }

    #[test]
    fn install_skill_force_overwrites_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "edited by user\n");
        let mut summary = Vec::new();
        install_skill(&path, "hello", true, false, &mut summary).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(summary[0].contains("force"));
    }

    #[test]
    fn install_skill_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        let mut summary = Vec::new();
        install_skill(&path, "hello", false, true, &mut summary).unwrap();
        assert!(!path.exists());
        assert!(summary[0].contains("write")); // reports the intent
    }

    #[test]
    fn merge_hook_creates_settings_with_claude_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook, false, &mut summary).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let stop = &v["hooks"]["Stop"];
        assert_eq!(stop.as_array().unwrap().len(), 1);
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "clank stop-hook --tool claude");
        let timeout = stop[0]["hooks"][0]["timeout"].as_u64().unwrap();
        assert_eq!(timeout, HOOK_TIMEOUT_SECS);
        // Claude entries don't carry statusMessage.
        assert!(stop[0]["hooks"][0].get("statusMessage").is_none());
    }

    #[test]
    fn merge_hook_codex_carries_status_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".codex/hooks.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, CodexHook, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let msg = v["hooks"]["Stop"][0]["hooks"][0]["statusMessage"]
            .as_str()
            .unwrap();
        assert!(msg.contains("Clank"));
    }

    #[test]
    fn merge_hook_preserves_unrelated_stop_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        // Seed with an existing unrelated Stop hook.
        write_file(
            &path,
            r#"{
                "hooks": {
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": "/my/other-tool --hook" }] }
                    ]
                }
            }"#,
        );
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "expected both hooks present, got: {v}");
        let commands: Vec<&str> = stop
            .iter()
            .map(|w| w["hooks"][0]["command"].as_str().unwrap())
            .collect();
        assert!(commands.contains(&"/my/other-tool --hook"));
        assert!(commands.contains(&"clank stop-hook --tool claude"));
    }

    #[test]
    fn merge_hook_replaces_prior_clank_entry_on_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook, false, &mut summary).unwrap();
        merge_hook_into_settings(&path, ClaudeHook, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "re-run should not duplicate; got: {v}");
    }

    #[test]
    fn merge_hook_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook, true, &mut summary).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn wrapper_is_clank_matches_exact_command() {
        let w = serde_json::json!({
            "hooks": [{"type":"command","command":"clank stop-hook --tool claude"}]
        });
        assert!(wrapper_is_clank(&w));
    }

    #[test]
    fn wrapper_is_clank_matches_prefix_with_extra_args() {
        let w = serde_json::json!({
            "hooks": [{"type":"command","command":"clank stop-hook --tool codex --debug"}]
        });
        assert!(wrapper_is_clank(&w));
    }

    #[test]
    fn wrapper_is_clank_rejects_unrelated() {
        let w = serde_json::json!({
            "hooks": [{"type":"command","command":"/some/other-tool"}]
        });
        assert!(!wrapper_is_clank(&w));
    }
}
