//! `clank setup` — install user-scope clank assets into
//! `~/.claude/` and `~/.codex/`.
//!
//! Writes skill files (refuse-if-drifted; D8) and
//! tag-merges a `Stop` hook entry into each agent's user-wide
//! hook config (idempotent by stable `id` marker; D8).
//!
//! Per the plan (D1) clank ships as a CLI that owns these
//! files — no separate plugin distribution. Re-running `clank
//! setup` after upgrading the binary refreshes everything.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::SetupArgs;

// Pub so `clank doctor` can compare on-disk skill files against
// the expected embedded content without recomputing the paths.
pub const CLAUDE_SKILL_BODY: &str = include_str!("setup_assets/claude_skill.md");
pub const CODEX_SKILL_BODY: &str = include_str!("setup_assets/codex_skill.md");

/// Stable identifier we write onto every clank-owned hook entry
/// as `"id": "<HOOK_ID>"`. The plan's D8 ownership model says
/// re-setup must find clank's entries by a stable marker — not
/// by the `command` string — so renames / absolute-path wrappers
/// / env shims don't strand prior entries and pile up duplicates.
/// Neither claude nor codex's hook schema rejects unknown
/// fields, so the marker rides along harmlessly.
const HOOK_ID: &str = "clank-stop-hook";

/// Legacy ownership signal: entries written before [`HOOK_ID`]
/// landed in this command were identified by a `command` that
/// started with this prefix. We still match on it so a re-setup
/// after upgrading from an earlier dogfood build replaces those
/// entries in place. Drop once nobody has un-upgraded configs.
const LEGACY_COMMAND_PREFIX: &str = "clank stop-hook";

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
    install_codex_rule(
        &home.join(".codex/rules/default.rules"),
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

/// The line we ensure is present in codex's command-rules file.
/// Bare prefix `["clank"]` whitelists every `clank <subcommand>`
/// without enumerating each one — the gate state machine itself
/// is what enforces "did this agent have the right to do that."
///
/// Format is codex's `prefix_rule(pattern=[...], decision="allow|deny")`
/// DSL.
const CODEX_RULE_LINE: &str = r#"prefix_rule(pattern=["clank"], decision="allow")"#;

/// Substring used to identify ANY rule whose pattern is the
/// single-element list `["clank"]`. The closing `]` immediately
/// after `"clank"` distinguishes the bare pattern from longer
/// patterns like `["clank", "init"]` — those contain `, "init"]`
/// between the `"` and the `]` so this substring is absent.
const CODEX_BARE_CLANK_PATTERN: &str = r#"pattern=["clank"]"#;

/// Minimal structural validation of a `prefix_rule(` line. The
/// file is the user's; we don't try to be a full DSL parser.
/// What we DO catch: lines that announce themselves as
/// `prefix_rule(` calls but are missing the closing `)`, missing
/// the `pattern=[...]` argument, or missing an
/// `decision="allow|deny"` argument. Each of these would leave
/// codex with a broken rules file after our append.
fn validate_prefix_rule_line(path: &Path, lineno: usize, trimmed: &str) -> anyhow::Result<()> {
    let malformed = |reason: &str| -> anyhow::Error {
        anyhow::anyhow!(
            "{}:{}: line `{}` is malformed ({}); fix or remove it, then re-run `clank setup`",
            path.display(),
            lineno,
            trimmed,
            reason
        )
    };
    // Must close the call. Strip a trailing comment if codex
    // ever adds support; for now we trust line.trim().
    if !trimmed.ends_with(')') {
        return Err(malformed("missing closing `)`"));
    }
    if !trimmed.contains("pattern=[") {
        return Err(malformed("missing `pattern=[...]` argument"));
    }
    if !trimmed.contains(']') {
        return Err(malformed("`pattern=[...]` is missing its closing `]`"));
    }
    let has_allow = trimmed.contains(r#"decision="allow""#);
    let has_deny = trimmed.contains(r#"decision="deny""#);
    if !has_allow && !has_deny {
        return Err(malformed(
            r#"missing `decision="allow"` or `decision="deny"` argument"#,
        ));
    }
    Ok(())
}

/// Ensure the codex command-rules file contains a bare `clank`
/// allow rule.
///
/// Idempotent: if a matching `decision="allow"` rule already
/// exists, no-op.
///
/// Fail-closed: if a matching `decision="deny"` rule exists, the
/// user has explicitly denied the pattern and we error out
/// naming the offending line. Don't silently override.
///
/// Honors `--dry-run`: prints the intended action without
/// touching the filesystem.
fn install_codex_rule(path: &Path, dry_run: bool, summary: &mut Vec<String>) -> anyhow::Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading `{}`", path.display())));
        }
    };

    if let Some(content) = &existing {
        let mut existing_clank_allow_seen = false;
        for (idx, line) in content.lines().enumerate() {
            let lineno = idx + 1;
            let trimmed = line.trim();
            if !trimmed.starts_with("prefix_rule(") {
                continue;
            }
            // Any `prefix_rule(` line must be well-formed. The
            // user's rules file is their space; if it's already
            // broken, appending our line wouldn't fix it AND would
            // leave us as the most recent edit on a broken file.
            // Surface the diagnostic; let the user fix it manually.
            validate_prefix_rule_line(path, lineno, trimmed)?;

            if !trimmed.contains(CODEX_BARE_CLANK_PATTERN) {
                continue;
            }
            if trimmed.contains(r#"decision="deny""#) {
                anyhow::bail!(
                    "{}:{}: line `{}` denies the `clank` pattern; \
                     remove or change it before re-running `clank setup`",
                    path.display(),
                    lineno,
                    trimmed,
                );
            }
            if trimmed.contains(r#"decision="allow""#) {
                existing_clank_allow_seen = true;
            }
        }
        if existing_clank_allow_seen {
            summary.push(format!("  ok    {}", path.display()));
            return Ok(());
        }
    }

    if !dry_run {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating `{}`", parent.display()))?;
        }
        let mut new_content = existing.unwrap_or_default();
        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push_str(CODEX_RULE_LINE);
        new_content.push('\n');
        std::fs::write(path, new_content)
            .with_context(|| format!("writing `{}`", path.display()))?;
    }
    summary.push(format!(
        "  write {} (append `clank` allow rule)",
        path.display()
    ));
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
/// JSON. Replaces any existing entry whose inner hook has the
/// stable [`HOOK_ID`] (or matches [`LEGACY_COMMAND_PREFIX`] for
/// pre-id-marker dogfood entries); preserves every unrelated
/// Stop hook.
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

    // Remove any prior clank entries — keyed on the stable
    // `id` marker first, falling back to the legacy
    // command-prefix match for entries written before HOOK_ID
    // shipped (dogfood-era).
    let before = stop.len();
    stop.retain(|wrapper| !wrapper_is_clank(wrapper));
    let removed = before - stop.len();

    // Add our fresh entry, tagged with HOOK_ID so future re-runs
    // (with whatever command shape we evolve to) can still find
    // and replace it.
    let mut inner_hook = serde_json::json!({
        "id": HOOK_ID,
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
    inner.iter().any(|h| {
        // Primary: the stable id marker.
        let id_match = h
            .get("id")
            .and_then(|v| v.as_str())
            .is_some_and(|id| id == HOOK_ID);
        // Legacy: pre-id-marker dogfood entries.
        let cmd_match = h
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|cmd| cmd.starts_with(LEGACY_COMMAND_PREFIX));
        id_match || cmd_match
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `- **FINISHED**:` bullet through (exclusive) the next
    /// `- **REQUEST_CHANGES**:` bullet.
    fn finished_block(skill: &str) -> &str {
        let start = skill
            .find("- **FINISHED**:")
            .expect("skill has a FINISHED bullet");
        let rest = &skill[start..];
        let end = rest
            .find("- **REQUEST_CHANGES**:")
            .expect("skill has a REQUEST_CHANGES bullet after FINISHED");
        &rest[..end]
    }

    #[test]
    fn claude_codex_skills_define_finished_identically() {
        // Divergence guard (finished-means-impl-done-not-plan-text,
        // ruthless): the FINISHED definition MUST be byte-identical
        // across the claude and codex skill assets, so the two
        // agents never drift on what FINISHED means. Editing one
        // skill's FINISHED bullet without the other fails here.
        assert_eq!(
            finished_block(CLAUDE_SKILL_BODY),
            finished_block(CODEX_SKILL_BODY),
            "claude and codex skills must define FINISHED identically"
        );
        // And the definition must be the implementation-complete
        // one, not the old plan-text-done phrasing.
        assert!(
            finished_block(CLAUDE_SKILL_BODY).contains("fully\n    implemented and merge-ready"),
            "FINISHED must mean the work is implemented, not the plan text written"
        );
    }

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
    fn embedded_skills_use_scoped_block_create_recipe() {
        // Regression for block-create-explicit-scope: both skill
        // assets used to embed `clank block create <name> -m
        // "question"` with no scope flag. After CLI flipped to
        // require explicit scope, that recipe became a no-op
        // error. Lock in the scoped recipe so future skill
        // edits don't silently regress.
        for (name, body) in [
            ("CLAUDE_SKILL_BODY", CLAUDE_SKILL_BODY),
            ("CODEX_SKILL_BODY", CODEX_SKILL_BODY),
        ] {
            assert!(
                body.contains("clank block create"),
                "{name} should still document block create"
            );
            assert!(
                body.contains("--plan"),
                "{name} must document `--plan` as the primary scope flag; \
                 forgetting it makes the recipe error at runtime"
            );
            // The bare invocation pattern (with no flag immediately
            // after the name) is what regressed. Guard against it
            // returning verbatim.
            assert!(
                !body.contains(r#"clank block create <name> -m "question""#),
                "{name} must not embed the legacy no-scope recipe"
            );
        }
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
        let inner = &stop[0]["hooks"][0];
        let id = inner["id"].as_str().unwrap();
        assert_eq!(id, HOOK_ID, "stable id marker must be present");
        let cmd = inner["command"].as_str().unwrap();
        assert_eq!(cmd, "clank stop-hook --tool claude");
        let timeout = inner["timeout"].as_u64().unwrap();
        assert_eq!(timeout, HOOK_TIMEOUT_SECS);
        // Claude entries don't carry statusMessage.
        assert!(inner.get("statusMessage").is_none());
    }

    #[test]
    fn merge_hook_replaces_entry_keyed_by_id_even_if_command_changed() {
        // Simulates upgrading: prior version wrote a hook with a
        // different command shape (e.g. absolute path) but our
        // id marker. Re-setup must replace by id, not by command.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        write_file(
            &path,
            &format!(
                r#"{{
                    "hooks": {{
                        "Stop": [
                            {{ "hooks": [
                                {{
                                    "id": "{HOOK_ID}",
                                    "type": "command",
                                    "command": "/some/abs/path/to/clank --baroque-flags stop-hook --tool claude",
                                    "timeout": 60
                                }}
                            ]}}
                        ]
                    }}
                }}"#
            ),
        );
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop.len(),
            1,
            "expected the prior entry replaced, not duplicated"
        );
        let inner = &stop[0]["hooks"][0];
        // New command shape; old absolute-path command gone.
        assert_eq!(
            inner["command"].as_str().unwrap(),
            "clank stop-hook --tool claude"
        );
        assert_eq!(
            inner["timeout"].as_u64().unwrap(),
            HOOK_TIMEOUT_SECS,
            "timeout refreshed from the binary's constant"
        );
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
    fn wrapper_is_clank_matches_id_marker() {
        let w = serde_json::json!({
            "hooks": [{
                "id": HOOK_ID,
                "type": "command",
                "command": "/totally/different/path/to/clank-stop-hook"
            }]
        });
        assert!(wrapper_is_clank(&w));
    }

    #[test]
    fn wrapper_is_clank_matches_legacy_command_prefix() {
        // Pre-id-marker dogfood entries — no `id` field at all.
        let w = serde_json::json!({
            "hooks": [{"type":"command","command":"clank stop-hook --tool claude"}]
        });
        assert!(wrapper_is_clank(&w));
    }

    #[test]
    fn wrapper_is_clank_matches_legacy_prefix_with_extra_args() {
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

    #[test]
    fn wrapper_is_clank_rejects_id_pointing_at_different_tool() {
        // A different tool that also happens to use an `id` field
        // but with a non-clank value must NOT be claimed by us.
        let w = serde_json::json!({
            "hooks": [{
                "id": "some-other-tool",
                "type": "command",
                "command": "/path/to/something"
            }]
        });
        assert!(!wrapper_is_clank(&w));
    }
}
