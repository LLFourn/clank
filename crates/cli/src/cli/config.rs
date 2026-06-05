use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::HookEvent;
use clank_core::agent_config::LaunchConfig;
use clank_core::ids::AgentLabel;
use clank_core::vocab::{Role, Tool};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub review: ReviewConfig,
    /// `Some(cmd)` = hook set, `None` = explicitly disabled (null).
    /// Absent keys are not in the map at all.
    pub hooks: BTreeMap<HookEvent, Option<String>>,
    /// `clank diff` settings — editor launch profile + default
    /// wait behavior. Layered field-by-field via `apply_layer`
    /// (matches `review`/`hooks`; NOT REPLACE like `agents`).
    pub diff: DiffConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            review: ReviewConfig::default(),
            hooks: BTreeMap::new(),
            diff: DiffConfig::default(),
        }
    }
}

/// `clank diff` config: editor launch profile + default wait
/// behavior. Loaded from `.clank/config.json#/diff` (user and
/// repo scopes, layered field-by-field).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffConfig {
    /// Editor launch profile. `None` means no editor is
    /// configured; `clank diff` errors with a message naming
    /// `diff.editor.command`. Default-empty avoids accidentally
    /// launching `$EDITOR` (which users may set for git commit
    /// message editing but not as their diff review surface).
    pub editor: Option<LaunchConfig>,
    /// Default `--wait` behavior. `Some(true)` = wait by default
    /// (`--no-wait` overrides). `Some(false)` = fire-and-forget
    /// (`--wait` overrides). `None` = fire-and-forget (system
    /// default per lloyd's wording: "otherwise it just opens and
    /// continues").
    pub wait: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewConfig {
    pub adhoc_feedback: bool,
    pub plan_feedback: bool,
    pub require_commit_prefix: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            adhoc_feedback: false,
            plan_feedback: true,
            require_commit_prefix: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    review: Option<ReviewFile>,
    #[serde(default)]
    hooks: Option<HooksFile>,
    #[serde(default)]
    diff: Option<DiffFile>,
}

/// Lossy-deserialize wrapper for `.clank/config.json#/diff`. Used
/// by `apply_layer`. The corresponding round-trip
/// serialize-capable type lives in `RepoConfigFile.extra` for now
/// — `clank agent add/remove/set-role` doesn't write the `diff`
/// section, so we don't need an explicit Serialize variant yet.
#[derive(Debug, Default, Deserialize)]
struct DiffFile {
    #[serde(default)]
    editor: Option<LaunchConfig>,
    #[serde(default)]
    wait: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct ReviewFile {
    #[serde(default, alias = "force_review_on_misc_commits")]
    adhoc_feedback: Option<bool>,
    #[serde(default, alias = "force_review_on_plan_commits")]
    plan_feedback: Option<bool>,
    #[serde(default)]
    require_commit_prefix: Option<bool>,
}

#[derive(Debug)]
struct HooksFile {
    entries: BTreeMap<String, serde_json::Value>,
}

impl<'de> Deserialize<'de> for HooksFile {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map: BTreeMap<String, serde_json::Value> = Deserialize::deserialize(deserializer)?;
        Ok(HooksFile { entries: map })
    }
}

impl Default for HooksFile {
    fn default() -> Self {
        HooksFile {
            entries: BTreeMap::new(),
        }
    }
}

impl HooksFile {
    fn get(&self, event: HookEvent) -> Option<Option<String>> {
        let key_underscore = event_to_key_name(event);
        let key_kebab = event.as_str();
        let val = self
            .entries
            .get(key_underscore)
            .or_else(|| self.entries.get(key_kebab))?;
        match val {
            serde_json::Value::Null => Some(None),
            serde_json::Value::String(s) => Some(Some(s.clone())),
            _ => None,
        }
    }
}

/// One entry in the merged agent declaration. Lives in both the
/// user-scope `~/.clank/config.json` `default_agents` list and the
/// repo-scope `<repo>/.clank/config.json` `agents` list. The
/// merged set is the source of truth for "which agents exist + their
/// role + tool + launch profile" (per `agent-add-cli-and-repo-scope`).
/// Per-agent skeletons at `.clank/agents/<label>/config.json` hold
/// only per-machine state (auto_mode, wfw_timeout, session).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DefaultAgent {
    pub label: AgentLabel,
    #[serde(default)]
    pub role: Role,
    /// Tool this agent runs (claude / codex). Consumed by spawners
    /// (zellij layouts) and by `clank agent start` as a fallback
    /// when the session is unbound. `None` = no preferred tool
    /// declared (rare; most adds use `--tool`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<Tool>,
    /// Launch profile (command override + args + env). Same shape
    /// as `clank_core::agent_config::LaunchConfig`. Consumed by
    /// `clank agent start` to compose the executed command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
}

/// Repo-scope `<repo>/.clank/config.json` deserialization wrapper
/// for the `agents` field. Public so tests + the `clank agent
/// add/remove/set-role` writers can round-trip via serde rather
/// than hand-rolling JSON.
///
/// `agents` is `Option<Vec>` (NOT `Vec`) — presence-aware. `None`
/// = key absent (caller falls back to user-scope). `Some(vec)` =
/// key present (even if empty). `Some(vec![])` serializes as
/// `"agents": []` and counts as an explicit override per
/// [`load_repo_agents`]'s semantics. Codex caught the `Vec` +
/// `skip_serializing_if = "Vec::is_empty"` conflation on da71c84.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RepoAgentsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<DefaultAgent>>,
}

/// User-scope `~/.clank/config.json` deserialization wrapper for
/// the `default_agents` field. Public for the same reason as
/// [`RepoAgentsFile`].
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct UserAgentsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agents: Option<Vec<DefaultAgent>>,
}

/// Round-trip typed schema for `<repo>/.clank/config.json`.
///
/// Used by `clank agent add/remove/set-role` (Phase 4 of
/// `agent-add-cli-and-repo-scope`) for read-modify-write: deserialize
/// the file, edit one field, reserialize. The `extra` flatten
/// catchall preserves unknown top-level keys so a newer clank's
/// config keys don't get wiped by an older clank's `agent add`.
///
/// **NOTE**: the `apply_layer` path (the lossy read-only loader used
/// by [`load`]) still uses its own private structs because it has
/// different failure semantics. The two paths are NOT unified yet —
/// see the queued `typed-config-dogfood` plan.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RepoConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewSection>,
    /// Raw hook entries — string or null. Kept as Value because
    /// the existing `HooksFile` lossy-loader handles both shapes;
    /// the write path just preserves whatever was there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, serde_json::Value>,
    /// `None` = `agents` key absent (caller falls back to
    /// user-scope). `Some(vec)` = key present (even if empty).
    /// Same presence semantics as [`load_repo_agents`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<DefaultAgent>>,
    /// Forward-compat catchall: any top-level key this version
    /// of clank doesn't know about. Preserved on round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Round-trip typed schema for `~/.clank/config.json`. Same shape
/// as [`RepoConfigFile`] but with `default_agents` instead of
/// `agents`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct UserConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewSection>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agents: Option<Vec<DefaultAgent>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Round-trip variant of [`ReviewFile`] (which is Deserialize-only).
/// Same field names + serde aliases so a config written by the
/// round-trip path stays readable by the lossy `apply_layer` path.
///
/// Legacy aliases `force_review_on_misc_commits` /
/// `force_review_on_plan_commits` are accepted on deserialize
/// (matching [`ReviewFile`]) and **canonicalized** on
/// reserialize (output always uses the modern names). Codex
/// caught the missing aliases on 9218aa9 — without them, a
/// config using the legacy keys would lose its review settings
/// after any agent-mutation round-trip.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ReviewSection {
    #[serde(
        default,
        alias = "force_review_on_misc_commits",
        skip_serializing_if = "Option::is_none"
    )]
    pub adhoc_feedback: Option<bool>,
    #[serde(
        default,
        alias = "force_review_on_plan_commits",
        skip_serializing_if = "Option::is_none"
    )]
    pub plan_feedback: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_commit_prefix: Option<bool>,
}

/// Strict loader for the user-scope `default_agents` list.
///
/// **Scope**: user-scope only. For the merged set (user-scope ∪
/// repo-scope-overrides), use [`load_merged_agents`]. This function
/// remains for the `clank init` path where repo-scope config
/// doesn't yet exist.
///
/// **Failure policy**: missing file = empty list (no opt-in = no
/// seed); malformed file = error. This is deliberately stricter
/// than the main `apply_layer` path, which is lossy
/// (logs+ignores malformed JSON) because review/hooks settings can
/// safely fall back to defaults. `default_agents` cannot fall back
/// safely: an empty list under the all-reviewers gate means
/// "master-only repo, auto-approve every commit". A silently
/// dropped `default_agents` would convert a multi-reviewer setup
/// into auto-finalize. Fail-closed here even though the rest of
/// the config layer is lossy.
pub fn load_default_agents(home: Option<&Path>) -> anyhow::Result<Vec<DefaultAgent>> {
    let Some(home) = home else {
        return Ok(Vec::new());
    };
    let path = home.join(".clank/config.json");
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(e))
                .with_context(|| format!("reading user config {}", path.display()));
        }
    };
    let parsed: UserAgentsFile = serde_json::from_str(&body)
        .with_context(|| format!("parsing user config {}", path.display()))?;
    Ok(parsed.default_agents.unwrap_or_default())
}

/// Presence-aware loader for the repo-scope `agents` list at
/// `<repo>/.clank/config.json`. Returns:
/// - `Ok(None)` — config file absent OR present without an `agents`
///   key. Caller should fall back to user-scope.
/// - `Ok(Some(vec))` — `agents` key present (even if empty `[]`).
///   Caller treats this as the authoritative set; `Some(vec![])`
///   explicitly overrides user-scope with the empty set (codex
///   caught the conflation on eef4c49).
///
/// Same failure-policy as [`load_default_agents`]: malformed JSON
/// errors; missing file is the empty case.
pub fn load_repo_agents(repo_root: &Path) -> anyhow::Result<Option<Vec<DefaultAgent>>> {
    let path = repo_root.join(".clank/config.json");
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::anyhow!(e))
                .with_context(|| format!("reading repo config {}", path.display()));
        }
    };
    // Use Value to detect key presence — the typed-struct path
    // can't distinguish "absent" from "explicit empty list."
    let value: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing repo config {}", path.display()))?;
    let Some(agents_value) = value.get("agents") else {
        return Ok(None);
    };
    let agents: Vec<DefaultAgent> = serde_json::from_value(agents_value.clone())
        .with_context(|| format!("parsing `agents` in {}", path.display()))?;
    Ok(Some(agents))
}

/// Declaration-only merge: repo-scope `agents` if present, else
/// user-scope `default_agents`. NO legacy skeleton fallback.
///
/// Used by paths that specifically need to know "what did the user
/// explicitly declare?" — `clank init`'s `seed_default_agents`
/// (legacy skeletons aren't things the user asked to seed) and
/// `clank agent add`'s cross-scope collision pre-check.
///
/// [`load_merged_agents`] is the right choice for gate / runtime
/// paths that need to see ALL registered agents including legacy.
pub fn load_declared_agents(
    repo_root: &Path,
    home: Option<&Path>,
) -> anyhow::Result<Vec<DefaultAgent>> {
    if let Some(repo) = load_repo_agents(repo_root)? {
        return Ok(repo);
    }
    load_default_agents(home)
}

/// Merged agent declaration: repo-scope `agents` if present (REPLACE
/// semantics — `the local project can modify it`); else fall back
/// to user-scope `default_agents`; else fall back to scanning the
/// per-agent skeleton directories at `<repo>/.clank/agents/*/`.
/// This is the source of truth for "which agents exist + their
/// role + tool + launch profile" per `agent-add-cli-and-repo-scope`.
///
/// **Three fallback layers** (codex review 2 of da71c84):
/// 1. Repo-scope `<repo>/.clank/config.json` `agents` key present
///    (even if empty) → use it.
/// 2. User-scope `~/.clank/config.json` `default_agents` non-empty
///    → use it.
/// 3. **Legacy fallback**: skeleton dirs at `.clank/agents/*/`
///    scanned and synthesized into `DefaultAgent` entries. Preserves
///    the plan's "existing repos keep working" promise — repos that
///    pre-date this plan have skeletons but no declaration, and
///    must still surface their registered agents to the gate.
///    `tool` + `launch` fields stay None (skeleton doesn't carry
///    those post-Phase-3); `role` comes from the skeleton's
///    legacy `role` field.
///
/// An EXPLICIT empty `agents: []` at repo-scope returns the empty
/// set (disables user-scope AND legacy fallbacks).
pub fn load_merged_agents(
    repo_root: &Path,
    home: Option<&Path>,
) -> anyhow::Result<Vec<DefaultAgent>> {
    if let Some(repo) = load_repo_agents(repo_root)? {
        return Ok(repo);
    }
    let user = load_default_agents(home)?;
    if !user.is_empty() {
        return Ok(user);
    }
    // Legacy fallback: synthesize from skeleton dirs. Preserves
    // pre-Phase-1 repos that have skeletons but no declaration.
    load_legacy_skeleton_agents(repo_root)
}

/// Scan `<repo>/.clank/agents/*/config.json` and synthesize
/// `DefaultAgent` entries from each skeleton's legacy `role` field.
/// Used by [`load_merged_agents`] as a fallback when neither
/// repo-scope `agents` nor user-scope `default_agents` are
/// configured.
fn load_legacy_skeleton_agents(repo_root: &Path) -> anyhow::Result<Vec<DefaultAgent>> {
    let agents_root = repo_root.join(".clank/agents");
    if !agents_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&agents_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(label) = AgentLabel::parse(name_str) else {
            continue;
        };
        let cfg_path = entry.path().join("config.json");
        let body = match std::fs::read_to_string(&cfg_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("reading {}", cfg_path.display()));
            }
        };
        // Use the typed AgentConfig deserialize — extracts the
        // legacy `role` field cleanly.
        let cfg: clank_core::agent_config::AgentConfig = serde_json::from_str(&body)
            .with_context(|| format!("parsing {}", cfg_path.display()))?;
        out.push(DefaultAgent {
            label,
            role: cfg.role,
            tool: cfg.session.as_ref().map(|s| s.tool),
            launch: cfg.launch.clone(),
        });
    }
    // Stable order.
    out.sort_by(|a, b| a.label.as_str().cmp(b.label.as_str()));
    Ok(out)
}

pub fn load(repo_root: &Path) -> Config {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    load_with_home(repo_root, home.as_deref())
}

fn load_with_home(repo_root: &Path, home: Option<&Path>) -> Config {
    let mut cfg = Config::default();
    let _ = apply_layer(
        &mut cfg,
        home.map(|h| h.join(".clank/config.json")).as_deref(),
    );
    let _ = apply_layer(&mut cfg, Some(&repo_root.join(".clank/config.json")));
    cfg
}

fn apply_layer(cfg: &mut Config, path: Option<&Path>) -> BTreeSet<String> {
    let mut present: BTreeSet<String> = BTreeSet::new();
    let Some(path) = path else { return present };
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return present,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "clank config: read failed; ignoring layer");
            return present;
        }
    };
    let parsed: ConfigFile = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "clank config: malformed JSON; ignoring layer");
            return present;
        }
    };
    if let Some(review) = parsed.review {
        if let Some(v) = review.adhoc_feedback {
            cfg.review.adhoc_feedback = v;
            present.insert("review.adhoc_feedback".to_string());
        }
        if let Some(v) = review.plan_feedback {
            cfg.review.plan_feedback = v;
            present.insert("review.plan_feedback".to_string());
        }
        if let Some(v) = review.require_commit_prefix {
            cfg.review.require_commit_prefix = v;
            present.insert("review.require_commit_prefix".to_string());
        }
    }
    if let Some(hooks) = parsed.hooks {
        for event in [
            HookEvent::MasterWork,
            HookEvent::ReviewerWork,
            HookEvent::PlanFinalized,
            HookEvent::Idle,
            HookEvent::Blocked,
        ] {
            if let Some(v) = hooks.get(event) {
                cfg.hooks.insert(event, v);
                present.insert(format!("hooks.{}", event_to_key_name(event)));
            }
        }
    }
    // Field-by-field diff layering: per OQ7 (clank-diff-editor),
    // repo-scope overrides user-scope at the field level. If
    // repo-scope sets diff.wait but not diff.editor, the editor
    // stays from user-scope. Each non-None field replaces the
    // current effective value.
    if let Some(diff) = parsed.diff {
        if let Some(editor) = diff.editor {
            // Track which sub-fields were present so the source
            // resolver can attribute them. Only `diff.editor.command`
            // is currently catalog-enumerated; args/env need
            // hand-editing per the catalog entry.
            if editor.command.is_some() {
                present.insert("diff.editor.command".to_string());
            }
            cfg.diff.editor = Some(editor);
        }
        if let Some(wait) = diff.wait {
            cfg.diff.wait = Some(wait);
            present.insert("diff.wait".to_string());
        }
    }
    present
}

// ── Key catalog ──────────────────────────────────────────────────────────────

pub struct KeyDef {
    pub section: &'static str,
    pub name: &'static str,
    pub type_desc: &'static str,
    pub default: &'static str,
    pub help: &'static str,
}

pub static KEY_CATALOG: &[KeyDef] = &[
    KeyDef {
        section: "review",
        name: "adhoc_feedback",
        type_desc: "bool",
        default: "false",
        help: "Require review for ad-hoc (non-plan) commits",
    },
    KeyDef {
        section: "review",
        name: "plan_feedback",
        type_desc: "bool",
        default: "true",
        help: "Require review for plan-attributed commits",
    },
    KeyDef {
        section: "review",
        name: "require_commit_prefix",
        type_desc: "bool",
        default: "false",
        help: "Require [plan] or [misc] commit title prefixes",
    },
    KeyDef {
        section: "hooks",
        name: "master_work",
        type_desc: "string|null",
        default: "null",
        help: "Shell command to run when master has new work",
    },
    KeyDef {
        section: "hooks",
        name: "reviewer_work",
        type_desc: "string|null",
        default: "null",
        help: "Shell command to run when a reviewer has work",
    },
    KeyDef {
        section: "hooks",
        name: "plan_finalized",
        type_desc: "string|null",
        default: "null",
        help: "Shell command to run when a plan is finished",
    },
    KeyDef {
        section: "hooks",
        name: "idle",
        type_desc: "string|null",
        default: "null",
        help: "Shell command to run on idle (no work)",
    },
    KeyDef {
        section: "hooks",
        name: "blocked",
        type_desc: "string|null",
        default: "null",
        help: "Shell command to run when an agent creates a block",
    },
    KeyDef {
        section: "diff",
        name: "editor.command",
        type_desc: "string|null",
        default: "null",
        help: "Editor executable for `clank diff`. Args/env need hand-editing in .clank/config.json under diff.editor.{args,env}",
    },
    KeyDef {
        section: "diff",
        name: "wait",
        type_desc: "bool",
        default: "false",
        help: "Default `--wait` behavior for `clank diff`; --no-wait/--wait override",
    },
];

// ── Source tracking ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueSource {
    Default,
    User,
    Repo,
}

impl std::fmt::Display for ValueSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueSource::Default => f.write_str("default"),
            ValueSource::User => f.write_str("user"),
            ValueSource::Repo => f.write_str("repo"),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
    pub source: ValueSource,
    pub description: &'static str,
}

pub fn resolve_key_values(repo_root: &Path) -> Vec<KeyValue> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_key_values_with_home(repo_root, home.as_deref())
}

fn resolve_key_values_with_home(repo_root: &Path, home: Option<&Path>) -> Vec<KeyValue> {
    let mut sources: BTreeMap<String, ValueSource> = BTreeMap::new();

    let mut user_cfg = Config::default();
    let user_present = apply_layer(
        &mut user_cfg,
        home.map(|h| h.join(".clank/config.json")).as_deref(),
    );

    let mut repo_cfg = user_cfg.clone();
    let repo_present = apply_layer(&mut repo_cfg, Some(&repo_root.join(".clank/config.json")));

    for key in &repo_present {
        sources.insert(key.clone(), ValueSource::Repo);
    }
    for key in &user_present {
        sources.entry(key.clone()).or_insert(ValueSource::User);
    }

    KEY_CATALOG
        .iter()
        .map(|def| {
            let key = format!("{}.{}", def.section, def.name);
            let value = get_value(&repo_cfg, &key);
            let source = sources.get(&key).copied().unwrap_or(ValueSource::Default);
            KeyValue {
                key,
                value,
                source,
                description: def.help,
            }
        })
        .collect()
}

fn hook_display(v: Option<&Option<String>>) -> String {
    match v {
        Some(Some(cmd)) => cmd.clone(),
        _ => "null".to_string(),
    }
}

pub fn get_value(cfg: &Config, key: &str) -> String {
    match key {
        "review.adhoc_feedback" => cfg.review.adhoc_feedback.to_string(),
        "review.plan_feedback" => cfg.review.plan_feedback.to_string(),
        "review.require_commit_prefix" => cfg.review.require_commit_prefix.to_string(),
        "hooks.master_work" => hook_display(cfg.hooks.get(&HookEvent::MasterWork)),
        "hooks.reviewer_work" => hook_display(cfg.hooks.get(&HookEvent::ReviewerWork)),
        "hooks.plan_finalized" => hook_display(cfg.hooks.get(&HookEvent::PlanFinalized)),
        "hooks.idle" => hook_display(cfg.hooks.get(&HookEvent::Idle)),
        "hooks.blocked" => hook_display(cfg.hooks.get(&HookEvent::Blocked)),
        "diff.editor.command" => cfg
            .diff
            .editor
            .as_ref()
            .and_then(|l| l.command.clone())
            .unwrap_or_else(|| "null".to_string()),
        "diff.wait" => cfg
            .diff
            .wait
            .map(|b| b.to_string())
            .unwrap_or_else(|| "null".to_string()),
        _ => "unknown key".to_string(),
    }
}

fn event_to_key_name(event: HookEvent) -> &'static str {
    match event {
        HookEvent::MasterWork => "master_work",
        HookEvent::ReviewerWork => "reviewer_work",
        HookEvent::PlanFinalized => "plan_finalized",
        HookEvent::Idle => "idle",
        HookEvent::Blocked => "blocked",
    }
}

/// Map a dotted catalog key to its JSON path. Slice depth varies:
/// most keys are two-level (`review.adhoc_feedback` → `["review",
/// "adhoc_feedback"]`); `diff.editor.command` is three-level
/// (`["diff", "editor", "command"]`).
fn key_to_json_path(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "review.adhoc_feedback" => Some(&["review", "adhoc_feedback"]),
        "review.plan_feedback" => Some(&["review", "plan_feedback"]),
        "review.require_commit_prefix" => Some(&["review", "require_commit_prefix"]),
        "hooks.master_work" => Some(&["hooks", "master_work"]),
        "hooks.reviewer_work" => Some(&["hooks", "reviewer_work"]),
        "hooks.plan_finalized" => Some(&["hooks", "plan_finalized"]),
        "hooks.idle" => Some(&["hooks", "idle"]),
        "hooks.blocked" => Some(&["hooks", "blocked"]),
        "diff.wait" => Some(&["diff", "wait"]),
        "diff.editor.command" => Some(&["diff", "editor", "command"]),
        _ => None,
    }
}

// ── run() ─────────────────────────────────────────────────────────────────────

use super::{ConfigArgs, ConfigKey, ConfigKeyArgs};

fn key_name(cmd: &ConfigKey) -> &'static str {
    match cmd {
        ConfigKey::ReviewAdhocFeedback(_) => "review.adhoc_feedback",
        ConfigKey::ReviewPlanFeedback(_) => "review.plan_feedback",
        ConfigKey::ReviewRequireCommitPrefix(_) => "review.require_commit_prefix",
        ConfigKey::HooksMasterWork(_) => "hooks.master_work",
        ConfigKey::HooksReviewerWork(_) => "hooks.reviewer_work",
        ConfigKey::HooksPlanFinalized(_) => "hooks.plan_finalized",
        ConfigKey::HooksIdle(_) => "hooks.idle",
        ConfigKey::HooksBlocked(_) => "hooks.blocked",
        ConfigKey::DiffEditorCommand(_) => "diff.editor.command",
        ConfigKey::DiffWait(_) => "diff.wait",
    }
}

fn key_args(cmd: &ConfigKey) -> &ConfigKeyArgs {
    match cmd {
        ConfigKey::ReviewAdhocFeedback(a)
        | ConfigKey::ReviewPlanFeedback(a)
        | ConfigKey::ReviewRequireCommitPrefix(a)
        | ConfigKey::HooksMasterWork(a)
        | ConfigKey::HooksReviewerWork(a)
        | ConfigKey::HooksPlanFinalized(a)
        | ConfigKey::HooksIdle(a)
        | ConfigKey::HooksBlocked(a)
        | ConfigKey::DiffEditorCommand(a)
        | ConfigKey::DiffWait(a) => a,
    }
}

pub async fn run(args: ConfigArgs) -> anyhow::Result<()> {
    let repo = match &args.repo {
        Some(p) => dunce::canonicalize(p)?,
        None => {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .output();
            match output {
                Ok(o) if o.status.success() => {
                    let root = String::from_utf8(o.stdout)?.trim().to_string();
                    dunce::canonicalize(std::path::Path::new(&root))?
                }
                _ => std::env::current_dir()?,
            }
        }
    };

    let Some(cmd) = &args.command else {
        if args.json {
            let cfg = load(&repo);
            let obj = serde_json::json!({
                "review": {
                    "adhoc_feedback": cfg.review.adhoc_feedback,
                    "plan_feedback": cfg.review.plan_feedback,
                    "require_commit_prefix": cfg.review.require_commit_prefix,
                },
                "hooks": {
                    "master_work": cfg.hooks.get(&HookEvent::MasterWork),
                    "reviewer_work": cfg.hooks.get(&HookEvent::ReviewerWork),
                    "plan_finalized": cfg.hooks.get(&HookEvent::PlanFinalized),
                    "idle": cfg.hooks.get(&HookEvent::Idle),
                    "blocked": cfg.hooks.get(&HookEvent::Blocked),
                },
                "diff": {
                    "editor": cfg.diff.editor,
                    "wait": cfg.diff.wait,
                }
            });
            println!("{}", serde_json::to_string_pretty(&obj)?);
        } else {
            let kvs = resolve_key_values(&repo);
            let key_w = kvs.iter().map(|kv| kv.key.len()).max().unwrap_or(0);
            let val_w = kvs.iter().map(|kv| kv.value.len()).max().unwrap_or(0);
            for kv in &kvs {
                println!(
                    "{:<kw$}  {:<vw$}  ({})  {}",
                    kv.key,
                    kv.value,
                    kv.source,
                    kv.description,
                    kw = key_w,
                    vw = val_w,
                );
            }
        }
        return Ok(());
    };

    let key = key_name(cmd);
    let kargs = key_args(cmd);
    let def = KEY_CATALOG
        .iter()
        .find(|d| format!("{}.{}", d.section, d.name) == key)
        .expect("key_name always returns a catalog key");

    match kargs.action.as_deref() {
        None => {
            let kvs = resolve_key_values(&repo);
            let kv = kvs.iter().find(|kv| kv.key == key);
            println!("key:     {}.{}", def.section, def.name);
            println!("type:    {}", def.type_desc);
            println!("default: {}", def.default);
            if let Some(kv) = kv {
                println!("value:   {}", kv.value);
                println!("source:  {}", kv.source);
            }
            println!("help:    {}", def.help);
        }
        Some("get") => {
            let cfg = load(&repo);
            println!("{}", get_value(&cfg, key));
        }
        Some("set") => {
            let value = kargs
                .value
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("set requires a value"))?;
            if def.type_desc == "bool" && value != "true" && value != "false" {
                anyhow::bail!("{key} is a bool; value must be true or false");
            }
            let path = key_to_json_path(key).expect("key_name always returns a catalog key");
            set_repo_key(&repo, path, value, def.type_desc)?;
            let kvs = resolve_key_values(&repo);
            if let Some(kv) = kvs.iter().find(|kv| kv.key == key) {
                println!("{} = {} ({})", kv.key, kv.value, kv.source);
            }
        }
        Some(action) => {
            anyhow::bail!("unknown action `{action}`; expected get or set");
        }
    }

    Ok(())
}

fn set_repo_key(repo: &Path, path: &[&str], value: &str, type_desc: &str) -> anyhow::Result<()> {
    if path.is_empty() {
        anyhow::bail!("empty JSON path");
    }
    let config_path = repo.join(".clank/config.json");
    let mut root: serde_json::Value = if config_path.exists() {
        let body = std::fs::read_to_string(&config_path)?;
        serde_json::from_str(&body)?
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };

    let json_val = match type_desc {
        "bool" => match value {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            _ => anyhow::bail!("bool key requires true or false"),
        },
        _ => {
            if value == "null" {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(value.to_string())
            }
        }
    };

    // Walk the path, creating intermediate objects as needed.
    let mut cursor = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config.json is not a JSON object"))?;
    for segment in &path[..path.len() - 1] {
        let entry = cursor
            .entry(*segment)
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        cursor = entry
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("config segment `{segment}` is not an object"))?;
    }
    cursor.insert(path.last().unwrap().to_string(), json_val);

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&root)?;
    std::fs::write(&config_path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn load_isolated(repo: &Path) -> Config {
        load_with_home(repo, None)
    }

    #[test]
    fn defaults_when_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_isolated(tmp.path());
        assert!(!cfg.review.adhoc_feedback);
        assert!(cfg.review.plan_feedback);
        assert!(!cfg.review.require_commit_prefix);
        assert!(cfg.hooks.is_empty());
    }

    #[test]
    fn key_catalog_defaults_match_review_config_default() {
        let defaults = ReviewConfig::default();
        let by_name = |name: &str| {
            KEY_CATALOG
                .iter()
                .find(|k| k.section == "review" && k.name == name)
                .unwrap_or_else(|| panic!("KEY_CATALOG missing review.{name}"))
        };
        assert_eq!(
            by_name("adhoc_feedback").default,
            defaults.adhoc_feedback.to_string()
        );
        assert_eq!(
            by_name("plan_feedback").default,
            defaults.plan_feedback.to_string()
        );
        assert_eq!(
            by_name("require_commit_prefix").default,
            defaults.require_commit_prefix.to_string()
        );
    }

    #[test]
    fn repo_layer_overrides_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".clank/config.json");
        write(&repo_cfg, r#"{"review": {"adhoc_feedback": false}}"#);
        let cfg = load_isolated(tmp.path());
        assert!(!cfg.review.adhoc_feedback);
        assert!(cfg.review.plan_feedback);
    }

    #[test]
    fn legacy_key_name_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".clank/config.json");
        write(
            &repo_cfg,
            r#"{"review": {"force_review_on_misc_commits": false}}"#,
        );
        let cfg = load_isolated(tmp.path());
        assert!(!cfg.review.adhoc_feedback);
    }

    #[test]
    fn malformed_json_ignored_layer_falls_through_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".clank/config.json");
        write(&repo_cfg, "not valid json");
        let cfg = load_isolated(tmp.path());
        assert!(cfg.review.plan_feedback);
        assert!(!cfg.review.adhoc_feedback);
    }

    #[test]
    fn hooks_in_config_json() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".clank/config.json");
        write(
            &repo_cfg,
            r#"{"hooks": {"master_work": "notify master", "idle": "do idle"}}"#,
        );
        let cfg = load_isolated(tmp.path());
        assert_eq!(
            cfg.hooks[&HookEvent::MasterWork],
            Some("notify master".to_string())
        );
        assert_eq!(cfg.hooks[&HookEvent::Idle], Some("do idle".to_string()));
    }

    #[test]
    fn hooks_kebab_alias_in_config_json() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".clank/config.json");
        write(&repo_cfg, r#"{"hooks": {"master-work": "kebab-cmd"}}"#);
        let cfg = load_isolated(tmp.path());
        assert_eq!(
            cfg.hooks[&HookEvent::MasterWork],
            Some("kebab-cmd".to_string())
        );
    }

    #[test]
    fn set_repo_key_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".clank")).unwrap();
        set_repo_key(tmp.path(), &["review", "adhoc_feedback"], "false", "bool").unwrap();
        let cfg = load_isolated(tmp.path());
        assert!(!cfg.review.adhoc_feedback);
    }

    #[test]
    fn get_value_returns_null_string_for_unset_hook() {
        let cfg = Config::default();
        assert_eq!(get_value(&cfg, "hooks.master_work"), "null");
    }

    #[test]
    fn set_bool_key_with_non_bool_value_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let args = super::super::ConfigArgs {
            command: Some(super::super::ConfigKey::ReviewAdhocFeedback(
                super::super::ConfigKeyArgs {
                    action: Some("set".to_string()),
                    value: Some("banana".to_string()),
                },
            )),
            repo: Some(tmp.path().to_path_buf()),
            json: false,
        };
        let result = tokio::runtime::Runtime::new().unwrap().block_on(run(args));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bool"));
    }

    #[test]
    fn set_prints_new_effective_value() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".clank")).unwrap();
        set_repo_key(tmp.path(), &["review", "adhoc_feedback"], "false", "bool").unwrap();
        let kvs = resolve_key_values_with_home(tmp.path(), None);
        let kv = kvs
            .iter()
            .find(|kv| kv.key == "review.adhoc_feedback")
            .unwrap();
        assert_eq!(kv.value, "false");
        assert_eq!(kv.source, ValueSource::Repo);
    }

    #[test]
    fn key_only_shows_value_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{"review": {"adhoc_feedback": false}}"#,
        );
        let kvs = resolve_key_values_with_home(tmp.path(), None);
        let kv = kvs
            .iter()
            .find(|kv| kv.key == "review.adhoc_feedback")
            .unwrap();
        assert_eq!(kv.value, "false");
        assert_eq!(kv.source, ValueSource::Repo);
    }

    #[test]
    fn hook_set_true_writes_string_not_bool() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".clank")).unwrap();
        set_repo_key(tmp.path(), &["hooks", "master_work"], "true", "string|null").unwrap();
        let body = std::fs::read_to_string(tmp.path().join(".clank/config.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["hooks"]["master_work"],
            serde_json::Value::String("true".into())
        );
    }

    #[test]
    fn hook_set_null_writes_json_null() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".clank")).unwrap();
        set_repo_key(tmp.path(), &["hooks", "idle"], "null", "string|null").unwrap();
        let body = std::fs::read_to_string(tmp.path().join(".clank/config.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["hooks"]["idle"].is_null());
    }

    #[test]
    fn unknown_action_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let args = super::super::ConfigArgs {
            command: Some(super::super::ConfigKey::ReviewAdhocFeedback(
                super::super::ConfigKeyArgs {
                    action: Some("nope".to_string()),
                    value: None,
                },
            )),
            repo: Some(tmp.path().to_path_buf()),
            json: false,
        };
        let result = tokio::runtime::Runtime::new().unwrap().block_on(run(args));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown action"));
    }

    #[test]
    fn diff_config_defaults_when_section_absent() {
        // Test 2: .clank/config.json with no `diff` key → cfg.diff
        // is default (editor: None, wait: None).
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{"review": {"adhoc_feedback": true}}"#,
        );
        let cfg = load_with_home(tmp.path(), None);
        assert!(cfg.diff.editor.is_none());
        assert!(cfg.diff.wait.is_none());
    }

    #[test]
    fn diff_config_loads_editor_and_wait() {
        // Test 1: full DiffConfig round-trip via load (the public
        // loader is what callers actually use).
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{
                "diff": {
                    "editor": {
                        "command": "emacsclient",
                        "args": ["-c", "{patch_file}"],
                        "env": {"DISPLAY": ":0"}
                    },
                    "wait": true
                }
            }"#,
        );
        let cfg = load_with_home(tmp.path(), None);
        let editor = cfg.diff.editor.expect("editor present");
        assert_eq!(editor.command.as_deref(), Some("emacsclient"));
        assert_eq!(editor.args, vec!["-c".to_string(), "{patch_file}".into()]);
        assert_eq!(cfg.diff.wait, Some(true));
    }

    #[test]
    fn diff_config_repo_scope_overrides_user_scope_field_by_field() {
        // Test 3: user-scope editor=vim, wait=false; repo-scope
        // editor=emacsclient. Result: editor=emacsclient (overridden),
        // wait=false (untouched in repo-scope). OQ7's
        // field-by-field layering semantic.
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".clank/config.json"),
            r#"{
                "diff": {
                    "editor": {"command": "vim"},
                    "wait": false
                }
            }"#,
        );
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{
                "diff": {
                    "editor": {"command": "emacsclient"}
                }
            }"#,
        );
        let cfg = load_with_home(tmp.path(), Some(home.path()));
        let editor = cfg.diff.editor.expect("editor present");
        assert_eq!(editor.command.as_deref(), Some("emacsclient"));
        // wait was NOT overridden by repo-scope (repo-scope didn't
        // set it), so user-scope's value survives.
        assert_eq!(cfg.diff.wait, Some(false));
    }

    #[test]
    fn diff_config_malformed_ignored_per_apply_layer_semantics() {
        // Test 4: malformed JSON in the `diff` section logs a
        // warning and cfg.diff falls back to default. Same lossy
        // semantics as the rest of apply_layer (the entire layer
        // is skipped if parse fails).
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(".clank/config.json"),
            "{ this is not valid JSON",
        );
        let cfg = load_with_home(tmp.path(), None);
        assert!(cfg.diff.editor.is_none());
        assert!(cfg.diff.wait.is_none());
    }

    #[test]
    fn load_repo_agents_distinguishes_absent_from_explicit_empty() {
        // Codex review of eef4c49: REPLACE semantics require
        // `agents: []` to disable user-scope defaults, distinct
        // from `agents` key absent (which falls back to user-scope).
        let tmp = tempfile::tempdir().unwrap();
        // Absent: no agents key at all.
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{"review": {"adhoc_feedback": true}}"#,
        );
        let absent = load_repo_agents(tmp.path()).unwrap();
        assert!(absent.is_none(), "no agents key → None; got {absent:?}");

        // Explicit empty: agents key present, list empty.
        write(&tmp.path().join(".clank/config.json"), r#"{"agents": []}"#);
        let empty = load_repo_agents(tmp.path()).unwrap();
        assert_eq!(
            empty,
            Some(Vec::new()),
            "explicit `agents: []` → Some(vec![])"
        );
    }

    #[test]
    fn review_section_legacy_aliases_round_trip_canonical() {
        // Codex caught on 9218aa9: ReviewSection was missing the
        // legacy `force_review_on_misc_commits` /
        // `force_review_on_plan_commits` aliases ReviewFile has.
        // Round-trip path would lose these values on agent
        // mutation. Fix: aliases accepted on deserialize,
        // canonicalized on reserialize.
        let legacy_json = r#"{
            "force_review_on_misc_commits": true,
            "force_review_on_plan_commits": false
        }"#;
        let parsed: ReviewSection = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.adhoc_feedback, Some(true));
        assert_eq!(parsed.plan_feedback, Some(false));
        // Reserialize: canonical names.
        let out = serde_json::to_string(&parsed).unwrap();
        assert!(
            out.contains(r#""adhoc_feedback":true"#),
            "should canonicalize to adhoc_feedback; got: {out}"
        );
        assert!(
            out.contains(r#""plan_feedback":false"#),
            "should canonicalize to plan_feedback; got: {out}"
        );
        assert!(
            !out.contains("force_review_on"),
            "legacy keys must NOT appear in serialized output; got: {out}"
        );
    }

    #[test]
    fn repo_config_with_legacy_review_keys_round_trips() {
        // Full round-trip: a RepoConfigFile with legacy review
        // keys deserializes, reserializes with canonical keys,
        // and preserves the values. This is the path that
        // `clank agent add/remove/set-role` exercises.
        let legacy_json = r#"{
            "review": {
                "force_review_on_misc_commits": true,
                "force_review_on_plan_commits": true
            },
            "agents": [
                {"label": "alice", "role": "reviewers"}
            ]
        }"#;
        let parsed: RepoConfigFile = serde_json::from_str(legacy_json).unwrap();
        let review = parsed.review.as_ref().expect("review section present");
        assert_eq!(review.adhoc_feedback, Some(true));
        assert_eq!(review.plan_feedback, Some(true));
        let agents = parsed.agents.as_ref().expect("agents present");
        assert_eq!(agents.len(), 1);
        // Reserialize and confirm.
        let out = serde_json::to_string(&parsed).unwrap();
        assert!(out.contains(r#""adhoc_feedback":true"#));
        assert!(out.contains(r#""plan_feedback":true"#));
        assert!(!out.contains("force_review_on"));
    }

    #[test]
    fn repo_agents_file_round_trips_explicit_empty() {
        // Codex caught on da71c84: writers using the typed wrapper
        // must serialize `agents: []` for an explicit empty
        // override. Pre-fix the field was `Vec<DefaultAgent>` with
        // `skip_serializing_if = "Vec::is_empty"` → empty vecs
        // disappeared from JSON.
        let file = RepoAgentsFile {
            agents: Some(Vec::new()),
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(
            json.contains(r#""agents":[]"#),
            "empty override must serialize as `agents: []`; got: {json}"
        );
        let round_trip: RepoAgentsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip.agents, Some(Vec::new()));
    }

    #[test]
    fn repo_agents_file_round_trips_absent() {
        let file = RepoAgentsFile { agents: None };
        let json = serde_json::to_string(&file).unwrap();
        // Absent should NOT serialize the `agents` key.
        assert!(
            !json.contains("agents"),
            "absent must not serialize agents key; got: {json}"
        );
    }

    #[test]
    fn load_merged_agents_falls_back_to_skeleton_when_no_declaration() {
        // Codex caught on da71c84: existing repos with skeletons
        // but no declaration must keep working. The legacy
        // fallback synthesizes DefaultAgent entries from skeleton
        // configs.
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // Skeleton for `legacy-reviewer` — no declaration anywhere.
        let agent_path = tmp.path().join(".clank/agents/legacy-reviewer");
        std::fs::create_dir_all(&agent_path).unwrap();
        std::fs::write(
            agent_path.join("config.json"),
            r#"{"auto_mode":"off","role":"reviewers"}"#,
        )
        .unwrap();

        let merged = load_merged_agents(tmp.path(), Some(home.path())).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].label.as_str(), "legacy-reviewer");
        assert_eq!(merged[0].role, Role::Reviewers);
    }

    #[test]
    fn load_merged_agents_explicit_empty_repo_overrides_user_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // User-scope has reviewers.
        write(
            &home.path().join(".clank/config.json"),
            r#"{"default_agents":[{"label":"alice","role":"reviewers"}]}"#,
        );
        // Repo-scope explicitly empty: user-scope MUST NOT leak through.
        write(&tmp.path().join(".clank/config.json"), r#"{"agents": []}"#);
        let merged = load_merged_agents(tmp.path(), Some(home.path())).unwrap();
        assert!(
            merged.is_empty(),
            "explicit empty repo-scope must override user-scope; got {merged:?}"
        );
    }

    #[test]
    fn load_merged_agents_absent_repo_falls_back_to_user_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".clank/config.json"),
            r#"{"default_agents":[{"label":"alice","role":"reviewers"}]}"#,
        );
        // No repo-scope config at all.
        let merged = load_merged_agents(tmp.path(), Some(home.path())).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].label.as_str(), "alice");
    }

    #[test]
    fn load_merged_agents_repo_replaces_user_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".clank/config.json"),
            r#"{"default_agents":[{"label":"alice","role":"reviewers"}]}"#,
        );
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{"agents":[{"label":"bob","role":"master"}]}"#,
        );
        let merged = load_merged_agents(tmp.path(), Some(home.path())).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].label.as_str(),
            "bob",
            "repo-scope replaces user-scope"
        );
    }

    #[test]
    fn load_repo_agents_malformed_json_errors() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".clank/config.json"), "{ not json");
        let err = load_repo_agents(tmp.path()).expect_err("must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parsing") || msg.contains("repo config"),
            "diagnostic should mention parse failure; got: {msg}"
        );
    }

    #[test]
    fn explicit_bool_equal_to_default_shows_repo_source() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{"review": {"adhoc_feedback": true}}"#,
        );
        let kvs = resolve_key_values_with_home(tmp.path(), None);
        let kv = kvs
            .iter()
            .find(|kv| kv.key == "review.adhoc_feedback")
            .unwrap();
        assert_eq!(kv.source, ValueSource::Repo);
    }
}
