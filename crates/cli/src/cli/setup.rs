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
use clank_core::vocab::{Role, Tool};

use super::SetupArgs;

// The `clank` skill is split by ROLE, not by tool: one tool hosts
// multiple roles (claude runs both the master and a reviewer), so a
// per-tool skill can't be role-specific. Each role's SKILL.md is
// composed from a shared core + a role body (+ a claude-only slash
// command) with the few tool-specific bits substituted, so there is
// ONE source per role. See [`compose_skill`].
const SKILL_SHARED_CORE: &str = include_str!("setup_assets/skill_shared_core.md");
const SKILL_MASTER_BODY: &str = include_str!("setup_assets/skill_master.md");
const SKILL_REVIEWER_BODY: &str = include_str!("setup_assets/skill_reviewer.md");
const SKILL_SLASH_COMMAND: &str = include_str!("setup_assets/skill_slash_command.md");

/// PR-review mode skill (clank-pr-review-mode). Tool-neutral — the
/// gh incantations are identical for claude and codex — so one body
/// installs to both skill dirs.
pub const PR_REVIEW_SKILL_BODY: &str = include_str!("setup_assets/pr_review_skill.md");

/// The two role skills, by their `~/.<tool>/skills/<name>/` dir name.
const MASTER_SKILL: &str = "clank-master";
const REVIEWER_SKILL: &str = "clank-reviewer";
/// The pre-split single skill, removed on setup.
const OBSOLETE_SKILL: &str = "clank";

/// `description:` frontmatter — the PRIMARY lever for role selection.
/// Lead with the role guard ("Use ONLY when … Do NOT use …") and name
/// the other skill, so an agent never loads the wrong role's skill (and
/// runs commands it must never touch).
const MASTER_DESC: &str = "Clank multi-agent workflow, MASTER role. Use ONLY when you are the master in a clank repo (implement plan milestones, promote/finish plans, manage the roster). Do NOT use as a reviewer — use clank-reviewer instead.";
const REVIEWER_DESC: &str = "Clank multi-agent workflow, REVIEWER role. Use ONLY when you are a reviewer in a clank repo (review commits and write verdicts). Do NOT use as the master — use clank-master instead.";

/// How each tool's agents receive work (claude-stop-hook-minimal-hint):
/// claude parks an armed background `clank wait` whose completion wake
/// carries the items — the skill must teach the arm/act/re-arm loop.
/// Codex is driven by the Stop hook blocking with the items directly.
/// Both variants keep the shared hint sentence the tests pin.
const WORK_LOOP_CLAUDE: &str = "\
- **Keep a `clank wait` armed.** End every turn with `clank wait`
  running as its OWN background task (Bash, `run_in_background: true`).
  Its completion is your wake: act on the items it printed IMMEDIATELY,
  then re-arm `clank wait` and end your turn. If you stop with nothing
  armed, the Stop hook reminds you to arm one. Run `clank status` if you
  need more than the hint carries (short SHAs resolve wherever a `<sha>`
  is wanted).
  Each item is a one-line hint: kind, plan, short sha.
- **NEVER poll.** Do not run `clank wait` in the FOREGROUND and do not
  re-run `clank status` waiting for state to change. The armed
  background wait wakes you; do not spin.";

const WORK_LOOP_CODEX: &str = "\
- **Act on Stop-hook work IMMEDIATELY, then YIELD.** When the Stop hook
  hands you work, do it now. codex surfaces this as a
  `Stop hook (blocked) feedback:` message. Run `clank status` if you
  need more than the hint carries (short SHAs resolve wherever a `<sha>`
  is wanted).
  Each item is a one-line hint: kind, plan, short sha.
- **NEVER poll.** Do not loop on `clank wait` or re-run `clank status`
  waiting for state to change. STOP — the Stop hook re-invokes you when
  there is work. You WILL be woken; do not spin.";

/// Grok wakes on background-task completion like claude
/// (grok-first-class P1), but its hooks are passive — no reminder
/// nudge exists, so the arming discipline is carried entirely here.
const WORK_LOOP_GROK: &str = "\
- **Keep a `clank wait` armed.** End every turn with `clank wait`
  running as a background terminal command (`background: true`).
  Its completion wakes you: act on the items it printed IMMEDIATELY,
  then re-arm `clank wait` and end your turn. Nothing reminds you if
  you forget — arming the wait is YOUR responsibility, every turn. Run
  `clank status` if you need more than the hint carries (short SHAs
  resolve wherever a `<sha>` is wanted).
  Each item is a one-line hint: kind, plan, short sha.
- **NEVER poll.** Do not run `clank wait` in the foreground and do not
  re-run `clank status` waiting for state to change. The armed
  background wait wakes you; do not spin.";

/// Compose a role's `SKILL.md` for a tool from the shared single-source
/// fragments: frontmatter (role-guarded description) + a role-guard
/// line + shared core + the role body, plus the claude-only `/clank`
/// slash command. Tool differences (`{{SHELL}}`, the per-tool
/// `{{WORK_LOOP}}` work-delivery teaching) are substituted, never
/// duplicated. Pure — unit-tested without touching the filesystem.
pub fn compose_skill(role: Role, tool: Tool) -> String {
    let (name, description, body, other) = match role {
        Role::Master => (MASTER_SKILL, MASTER_DESC, SKILL_MASTER_BODY, REVIEWER_SKILL),
        Role::Reviewer => (
            REVIEWER_SKILL,
            REVIEWER_DESC,
            SKILL_REVIEWER_BODY,
            MASTER_SKILL,
        ),
    };
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {name}\n"));
    out.push_str(&format!("description: {description}\n"));
    out.push_str("---\n\n");
    out.push_str(&format!(
        "> ROLE GUARD: this skill is for the **{role}** role. If your role \
         in this repo is not {role}, stop and use `{other}` instead — your \
         role is what `clank status` and your work items report.\n\n",
        role = role.as_str(),
    ));
    out.push_str(SKILL_SHARED_CORE);
    out.push_str(body);
    if tool == Tool::Claude {
        out.push_str(SKILL_SLASH_COMMAND);
    }

    let shell = match tool {
        Tool::Claude => "Bash",
        Tool::Codex => "shell",
        Tool::Grok => "run_terminal_command",
    };
    let work_loop = match tool {
        Tool::Claude => WORK_LOOP_CLAUDE,
        Tool::Codex => WORK_LOOP_CODEX,
        Tool::Grok => WORK_LOOP_GROK,
    };
    out.replace("{{SHELL}}", shell)
        .replace("{{WORK_LOOP}}", work_loop)
}

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
/// 24 hours — effectively infinite. The clank-side `wait_timeout`
/// in the agent's local AgentConfig is the real timer; this just
/// stops the agent's hook runner from killing the process early.
const HOOK_TIMEOUT_SECS: u64 = 86400;

pub async fn run(args: SetupArgs) -> anyhow::Result<()> {
    let home = home_dir()?;
    let mut summary = Vec::<String>::new();

    // Role skills: clank-master + clank-reviewer, both to both tools
    // (tool != role). Composed per (role, tool) from one source each.
    // Grok reads native skills from ~/.grok/skills and dedupes by name
    // against its claude-compat scan, native winning (grok-first-class
    // P3) — so installing here cleanly overrides the claude copies it
    // would otherwise pick up. No grok hook is installed: grok hooks
    // are passive and the wake channel is the armed wait's completion.
    for (tool, tool_dir) in [
        (Tool::Claude, ".claude"),
        (Tool::Codex, ".codex"),
        (Tool::Grok, ".grok"),
    ] {
        for (role, skill) in [
            (Role::Master, MASTER_SKILL),
            (Role::Reviewer, REVIEWER_SKILL),
        ] {
            install_skill(
                &home.join(format!("{tool_dir}/skills/{skill}/SKILL.md")),
                &compose_skill(role, tool),
                args.force,
                args.dry_run,
                &mut summary,
            )?;
        }
        // Drop the pre-split single `clank` skill so it can't shadow the
        // role skills with stale, role-jamming guidance.
        remove_obsolete_skill(
            &home.join(format!("{tool_dir}/skills/{OBSOLETE_SKILL}")),
            args.dry_run,
            &mut summary,
        )?;
    }
    install_skill(
        &home.join(".claude/skills/clank-pr-review/SKILL.md"),
        PR_REVIEW_SKILL_BODY,
        args.force,
        args.dry_run,
        &mut summary,
    )?;
    install_skill(
        &home.join(".codex/skills/clank-pr-review/SKILL.md"),
        PR_REVIEW_SKILL_BODY,
        args.force,
        args.dry_run,
        &mut summary,
    )?;
    install_skill(
        &home.join(".grok/skills/clank-pr-review/SKILL.md"),
        PR_REVIEW_SKILL_BODY,
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

    // Seed the `finish.autosquash` default into the user-scope config on first
    // setup — absent → true; an explicit true/false is preserved. Pairs with
    // autosquash implying `allow_rewrite_protected` so it works on the natural
    // (often `master`) workflow branch.
    if let Some(line) = seed_autosquash_default(&home.join(".clank/config.json"), args.dry_run)? {
        summary.push(line);
    }
    if let Some(line) = seed_research_team(&home, args.dry_run)? {
        summary.push(line);
    }

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

/// Seed `finish.autosquash=true` into `user_config` on first setup. Returns
/// the summary line when it seeds (or would, under `dry_run`); `None` when the
/// key is already set (explicit true/false is preserved) or on a dry-run of an
/// already-set key. Split out for testing without `$HOME` mutation.
/// Seed the `research` team — grok master, codex commit reviewer,
/// claude final reviewer (grok-first-class M4) — into the user config
/// when no team of that name exists. An existing `research` team is
/// never touched: the seed is a starting point, not a managed entry.
fn seed_research_team(home: &Path, dry_run: bool) -> anyhow::Result<Option<String>> {
    use crate::cli::teams_config::{RosterAgent, RosterRole, TeamMember};
    use crate::lifecycle::AgentLabel;
    use clank_core::vocab::Tool;

    let mut cfg = crate::cli::team::read_user_config(home)?;
    if cfg.teams.contains_key("research") {
        return Ok(None);
    }
    let notice = "seeded team `research`: grok (master), codex (commit), claude (final)";
    if dry_run {
        return Ok(Some(format!("would have {notice}")));
    }
    let member = |tool: Tool, role: RosterRole| {
        TeamMember::Inline(RosterAgent {
            tool,
            launch: None,
            initial_prompt: None,
            role,
        })
    };
    let roster = [
        ("grok", member(Tool::Grok, RosterRole::Master)),
        ("codex", member(Tool::Codex, RosterRole::Commit)),
        ("claude", member(Tool::Claude, RosterRole::Final)),
    ]
    .into_iter()
    .map(|(l, m)| (AgentLabel::parse(l).expect("static label"), m))
    .collect();
    cfg.teams.insert("research".to_string(), roster);
    crate::cli::team::write_user_config(home, &cfg)?;
    Ok(Some(notice.to_string()))
}

fn seed_autosquash_default(user_config: &Path, dry_run: bool) -> anyhow::Result<Option<String>> {
    let key = &["finish", "autosquash"][..];
    // A destructive default must disclose itself at the surface, not just in a
    // code comment (ruthless 432a82e): say plainly that finish now rewrites the
    // current branch in place, the published-history risk, and the opt-outs.
    let notice = |verb: &str| {
        format!(
            "{verb} finish.autosquash=true (default): `clank finish` now collapses each plan \
             by REWRITING the current branch in place — on an already-pushed branch this \
             rewrites published history. Disable with `clank config finish.autosquash set false`, \
             or skip one finish with `--no-squash`."
        )
    };
    if dry_run {
        Ok(
            (!crate::cli::config::json_path_present(user_config, key))
                .then(|| notice("would seed")),
        )
    } else if crate::cli::config::set_key_if_absent(user_config, key, "true", "bool")? {
        Ok(Some(notice("seeded")))
    } else {
        Ok(None)
    }
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

/// Remove a clank-owned skill dir that the binary no longer ships
/// (the pre-split single `clank` skill). The dir is clank-owned, so we
/// remove it outright when its `SKILL.md` is present; a no-op when
/// already gone. Records the action in the summary.
fn remove_obsolete_skill(
    dir: &Path,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    if dir.join("SKILL.md").exists() {
        if !dry_run {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("removing obsolete skill `{}`", dir.display()))?;
        }
        summary.push(format!(
            "  remove {} (split into clank-master / clank-reviewer)",
            dir.display()
        ));
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

/// Our Stop-hook entry, merged into the user's `settings.local.json`
/// (typed-json-not-json-macro). Only the fragment WE contribute is
/// typed; the surrounding read-modify-write stays `Value`-level since
/// the file is user-owned.
#[derive(serde::Serialize)]
struct StopHookEntry<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    command: &'a str,
    timeout: u64,
    #[serde(rename = "statusMessage", skip_serializing_if = "Option::is_none")]
    status_message: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct StopHookWrapper<'a> {
    hooks: [StopHookEntry<'a>; 1],
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(e) => return Err(e.into()),
    };

    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    let hooks = obj
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` is not a JSON object"))?;
    let stop = hooks
        .entry("Stop".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
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
    let entry = StopHookEntry {
        id: HOOK_ID,
        kind: "command",
        command: kind.command(),
        timeout: HOOK_TIMEOUT_SECS,
        status_message: kind.status_message(),
    };
    stop.push(serde_json::to_value(StopHookWrapper { hooks: [entry] })?);

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

    #[test]
    fn seed_autosquash_seeds_when_absent_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".clank/config.json");
        // Absent → seeds, and the notice DISCLOSES the destructive default
        // (in-place rewrite + published-history risk + both opt-outs) at the
        // surface, not just in a code comment (ruthless 432a82e).
        let notice = seed_autosquash_default(&cfg, false).unwrap().unwrap();
        assert!(
            notice.contains("REWRITING the current branch in place"),
            "{notice}"
        );
        assert!(notice.contains("published history"), "{notice}");
        assert!(notice.contains("finish.autosquash set false"), "{notice}");
        assert!(notice.contains("--no-squash"), "{notice}");
        assert!(crate::cli::config::json_path_present(
            &cfg,
            &["finish", "autosquash"]
        ));
        // Already set → no-op (explicit value preserved).
        assert!(seed_autosquash_default(&cfg, false).unwrap().is_none());
    }

    #[test]
    fn seed_autosquash_dry_run_reports_but_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".clank/config.json");
        let line = seed_autosquash_default(&cfg, true).unwrap();
        assert!(line.unwrap().contains("would seed"));
        assert!(!cfg.exists(), "dry-run writes nothing");
    }

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
    fn finished_defined_once_in_reviewer_and_tool_independent() {
        // finished-means-impl-done-not-plan-text: FINISHED is a REVIEWER
        // verdict, defined in ONE source (the reviewer body), so the two
        // tools can't drift on what it means; the master never defines a
        // verdict. Single-source replaces the old per-tool equality guard.
        let rc = compose_skill(Role::Reviewer, Tool::Claude);
        let rx = compose_skill(Role::Reviewer, Tool::Codex);
        assert_eq!(
            finished_block(&rc),
            finished_block(&rx),
            "FINISHED must read identically across tools (single source)"
        );
        assert!(
            finished_block(&rc).contains("implemented and merge-ready"),
            "FINISHED must mean the work is implemented, not the plan text written"
        );
        assert!(
            !compose_skill(Role::Master, Tool::Claude).contains("- **FINISHED**:"),
            "the master skill must not define verdicts (reviewer-only)"
        );
    }

    #[test]
    fn grok_skill_teaches_the_armed_wait_loop_without_a_nudge_crutch() {
        // grok-first-class: grok wakes on background-task completion
        // (P1) but has NO nudge channel (passive hooks) — the skill
        // alone carries the arming discipline, in grok's vocabulary.
        let mg = compose_skill(Role::Master, Tool::Grok);
        assert!(mg.contains("background: true"), "grok bg phrasing");
        assert!(
            mg.contains("Nothing reminds you"),
            "the no-nudge discipline is stated"
        );
        assert!(
            mg.contains("run_terminal_command"),
            "grok shell name substituted"
        );
        assert!(!mg.contains("{{WORK_LOOP}}") && !mg.contains("{{SHELL}}"));
        assert!(
            !mg.contains("Stop hook (blocked) feedback:"),
            "codex-only phrase must not leak into grok"
        );
        assert!(
            !mg.contains("run_in_background: true"),
            "claude-only phrasing must not leak into grok"
        );
    }

    #[test]
    fn research_team_seeds_once_and_never_touches_an_existing_definition() {
        use crate::cli::teams_config::RosterRole;
        use crate::lifecycle::AgentLabel;
        let home = tempfile::tempdir().unwrap();

        // Dry run reports without writing.
        assert!(
            seed_research_team(home.path(), true).unwrap().is_some(),
            "dry-run announces the seed"
        );
        assert!(
            crate::cli::team::read_user_config(home.path())
                .unwrap()
                .teams
                .is_empty()
        );

        // Fresh home: seeds grok master / codex commit / claude final.
        seed_research_team(home.path(), false).unwrap().unwrap();
        let cfg = crate::cli::team::read_user_config(home.path()).unwrap();
        let team = cfg.teams.get("research").unwrap();
        let role_of = |l: &str| team.get(&AgentLabel::parse(l).unwrap()).unwrap().role();
        assert_eq!(role_of("grok"), RosterRole::Master);
        assert_eq!(role_of("codex"), RosterRole::Commit);
        assert_eq!(role_of("claude"), RosterRole::Final);

        // Idempotent, and an existing definition is a user decision —
        // never modified even if it drifts from the seed.
        assert!(seed_research_team(home.path(), false).unwrap().is_none());
        let mut cfg = crate::cli::team::read_user_config(home.path()).unwrap();
        let team = cfg.teams.get_mut("research").unwrap();
        team.remove(&AgentLabel::parse("claude").unwrap());
        crate::cli::team::write_user_config(home.path(), &cfg).unwrap();
        assert!(seed_research_team(home.path(), false).unwrap().is_none());
        let cfg = crate::cli::team::read_user_config(home.path()).unwrap();
        assert_eq!(
            cfg.teams.get("research").unwrap().len(),
            2,
            "user-modified team survives re-setup untouched"
        );
    }

    #[test]
    fn role_skills_are_lean_each_lacks_the_other_roles_commands() {
        for tool in [Tool::Claude, Tool::Codex] {
            let master = compose_skill(Role::Master, tool);
            let reviewer = compose_skill(Role::Reviewer, tool);
            // Master owns finish + roster; never writes verdicts.
            assert!(
                master.contains("clank finish"),
                "master must document finish"
            );
            assert!(
                master.contains("clank agent promote"),
                "master must document roster commands"
            );
            assert!(
                !master.contains("--verdict"),
                "master must not carry verdict-writing mechanics"
            );
            // Reviewer writes verdicts; never finishes/promotes.
            assert!(
                reviewer.contains("clank feedback write") && reviewer.contains("--verdict"),
                "reviewer must document the verdict-write command"
            );
            for verdict in ["CONTINUE", "FINISHED", "REQUEST_CHANGES"] {
                assert!(reviewer.contains(verdict), "reviewer must define {verdict}");
            }
            assert!(
                !reviewer.contains("clank finish") && !reviewer.contains("queue promote"),
                "reviewer must not carry master-only commands"
            );
        }
    }

    #[test]
    fn role_invariants_lead_each_body() {
        // Guard against silent drift of the behaviours this split exists
        // to encode.
        let master = compose_skill(Role::Master, Tool::Claude);
        assert!(
            master.contains("Commit → STOP → get woken") && master.contains("PROMOTING"),
            "master must encode the commit->yield loop incl. promotion"
        );
        let reviewer = compose_skill(Role::Reviewer, Tool::Claude);
        assert!(
            reviewer.contains("NEVER withhold CONTINUE or FINISHED waiting on a manual/external"),
            "reviewer must encode the committable-scope invariant"
        );
        assert!(
            reviewer.contains("ARCHITECTURE-FIRST"),
            "reviewer must encode architecture-first review"
        );
        for body in [&master, &reviewer] {
            assert!(
                body.contains("NEVER poll"),
                "both roles must forbid polling"
            );
        }
    }

    #[test]
    fn role_skills_mark_repo_config_local_and_gitignored() {
        // clank-config-local-only: generated skills must not teach agents
        // that `.clank/config.json` is a shared/tracked project artifact.
        for role in [Role::Master, Role::Reviewer] {
            for tool in [Tool::Claude, Tool::Codex, Tool::Grok] {
                let body = compose_skill(role, tool);
                assert!(
                    body.contains("`config.json` — local, gitignored repo config"),
                    "{role:?}/{tool:?} skill must mark config.json local-only"
                );
                assert!(
                    body.contains("repo ROSTER"),
                    "{role:?}/{tool:?} skill must still name the roster semantics"
                );
                assert!(
                    !body.contains("`config.json` — the repo ROSTER"),
                    "{role:?}/{tool:?} skill must not carry the legacy tracked-artifact wording"
                );
            }
        }
    }

    #[test]
    fn role_guard_descriptions_are_unambiguous_both_ways() {
        let m = compose_skill(Role::Master, Tool::Claude);
        assert!(m.contains("name: clank-master"));
        assert!(
            m.contains("Use ONLY when you are the master") && m.contains("use clank-reviewer"),
            "master description must guard its role and redirect the other"
        );
        let r = compose_skill(Role::Reviewer, Tool::Claude);
        assert!(r.contains("name: clank-reviewer"));
        assert!(
            r.contains("Use ONLY when you are a reviewer") && r.contains("use clank-master"),
            "reviewer description must guard its role and redirect the other"
        );
    }

    #[test]
    fn tool_specific_bits_are_substituted() {
        let mc = compose_skill(Role::Master, Tool::Claude);
        let mx = compose_skill(Role::Master, Tool::Codex);
        assert!(mc.contains("run via Bash"));
        assert!(mx.contains("run via shell"));
        assert!(
            !mc.contains("{{") && !mx.contains("{{"),
            "all tokens substituted"
        );
        // Slash command is claude-only.
        assert!(
            mc.contains("$ARGUMENTS"),
            "claude skill has the /clank slash command"
        );
        assert!(
            !mx.contains("$ARGUMENTS"),
            "codex skill omits the slash command"
        );
        // Codex carries the stop-hook phrasing note; claude does not.
        assert!(mx.contains("Stop hook (blocked) feedback:"));
        assert!(!mc.contains("Stop hook (blocked) feedback:"));
    }

    #[test]
    fn work_loop_teaches_the_per_tool_delivery_model() {
        // claude-stop-hook-minimal-hint: claude agents park an armed
        // background `clank wait` whose completion wake carries the
        // items; codex agents are driven by the Stop hook blocking
        // with the items directly.
        let mc = compose_skill(Role::Master, Tool::Claude);
        let mx = compose_skill(Role::Master, Tool::Codex);
        assert!(mc.contains("Keep a `clank wait` armed"));
        assert!(mc.contains("run_in_background"));
        assert!(mc.contains("re-arm"));
        assert!(!mc.contains("Act on Stop-hook work"));
        assert!(mx.contains("Act on Stop-hook work IMMEDIATELY"));
        assert!(!mx.contains("run_in_background"));
    }

    #[test]
    fn skills_teach_compose_from_hint_not_verbatim() {
        // wait-output-is-a-minimal-hint: the wake carries no verbatim
        // command; the shared core teaches composing from the hint.
        const HINT_SENTENCE: &str = "Each item is a one-line hint: kind, plan, short sha.";
        for role in [Role::Master, Role::Reviewer] {
            for tool in [Tool::Claude, Tool::Codex] {
                let body = compose_skill(role, tool);
                assert!(!body.contains("run it verbatim"), "no verbatim promise");
                assert!(
                    body.contains(HINT_SENTENCE),
                    "must teach composing from the one-line hint"
                );
            }
        }
    }

    #[test]
    fn pr_review_skill_carries_verbs_and_verified_incantations() {
        let body = PR_REVIEW_SKILL_BODY;
        assert!(
            body.starts_with("---\nname: clank-pr-review\n"),
            "skill needs the clank-pr-review frontmatter"
        );
        for verb in [
            "clank pr-review start",
            "clank pr-review propose",
            "clank pr-review note",
            "clank pr-review submit",
        ] {
            assert!(body.contains(verb), "skill must document `{verb}`");
        }
        // The verified gh paths (top-level add, threaded reply).
        assert!(
            body.contains("addPullRequestReviewThread"),
            "skill must give the verified top-level-add path"
        );
        assert!(
            body.contains("addPullRequestReviewComment") && body.contains("inReplyTo"),
            "skill must give the verified threaded-reply path"
        );
        // Guard the footgun: resolves use --paginate --jq, never
        // `--slurp --jq` (gh rejects that combination).
        assert!(
            !body.contains("--slurp --jq") && !body.contains("--jq --slurp"),
            "skill must not pair --slurp with --jq (gh rejects it)"
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
    fn master_skill_uses_scoped_block_create_recipe() {
        // block-create-explicit-scope: the recipe must carry `--plan`
        // (the bare no-scope form errors at runtime). `block create` is
        // a MASTER command — reviewers must not carry it.
        for tool in [Tool::Claude, Tool::Codex] {
            let body = compose_skill(Role::Master, tool);
            assert!(
                body.contains("clank block create"),
                "master must document block create"
            );
            assert!(
                body.contains("--plan"),
                "block create recipe must carry --plan (the primary scope flag)"
            );
            assert!(
                !body.contains(r#"clank block create <name> -m "question""#),
                "must not embed the legacy no-scope recipe"
            );
            assert!(
                !compose_skill(Role::Reviewer, tool).contains("clank block create"),
                "reviewer must not carry the master-only block-create command"
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
