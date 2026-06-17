use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clank_core::HookEvent;
use clank_core::agent_config::LaunchConfig;
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct DiffConfig {
    /// Editor launch profile. `None` means no editor is
    /// configured; `clank diff` errors with a message naming
    /// `diff.editor.command`. Default-empty avoids accidentally
    /// launching `$EDITOR` (which users may set for git commit
    /// message editing but not as their diff review surface).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<LaunchConfig>,
    /// Default `--wait` behavior. `Some(true)` = wait by default
    /// (`--no-wait` overrides). `Some(false)` = fire-and-forget
    /// (`--wait` overrides). `None` = fire-and-forget (system
    /// default per lloyd's wording: "otherwise it just opens and
    /// continues").
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
/// by `apply_layer`. Uses presence-aware Option fields on the
/// editor's sub-fields so field-by-field layering per OQ7 of
/// `clank-diff-editor` actually works — codex 8cccb87 caught
/// that replacing the whole LaunchConfig dropped user-scope
/// args/env when repo-scope set only command.
#[derive(Debug, Default, Deserialize)]
struct DiffFile {
    #[serde(default)]
    editor: Option<DiffEditorFile>,
    #[serde(default)]
    wait: Option<bool>,
}

/// Layer-specific editor file. Each field is `Option<T>` so we
/// can distinguish "absent from this layer" from "explicitly
/// empty." `apply_layer` merges field-by-field into
/// `cfg.diff.editor`.
#[derive(Debug, Default, Deserialize)]
struct DiffEditorFile {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
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

/// Round-trip typed schema for `<repo>/.clank/config.json`.
///
/// Carries the flat config sections (`review`, `hooks`, `diff`)
/// for `clank config` read-modify-write. NOTE: agent
/// registration moved out of this struct entirely
/// (`teams-based-agent-registration`); the `team` / `promoted`
/// fields live on `teams_config::RepoConfigFile` and any unknown
/// keys round-trip through the `extra` flatten below.
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
    /// Typed hook entries per `typed-config-dogfood`. Producers
    /// construct via `HooksSection` rather than `BTreeMap<String,
    /// Value>` so schema drift is a type error, not a silent
    /// round-trip drop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksSection>,
    /// `clank diff` settings — editor launch profile + default
    /// wait behavior. Round-tripped so `clank config` set
    /// operations preserve it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffConfig>,
    /// Forward-compat catchall: any top-level key this version
    /// of clank doesn't know about. Preserved on round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Typed `hooks` section for round-trip producers per
/// `typed-config-dogfood`. Each entry is `Option<Option<String>>`
/// so presence-aware semantics work:
/// - `Some(Some("cmd"))` = explicitly set
/// - `Some(None)` = explicitly null (disable)
/// - `None` = absent (use default)
///
/// The `extra` flatten catchall preserves unknown hook event
/// names on round-trip — ruthless caught the gap on b6323be
/// (`RepoConfigFile.extra` catches unknown SECTIONS at the top
/// level, NOT unknown FIELDS inside hooks; without per-section
/// `extra`, round-trip would silently drop forward-compat
/// hooks).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HooksSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_work: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_work: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_finalized: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<Option<String>>,
    /// Forward-compat catchall for hook event names this build
    /// doesn't know about yet. `serde_json::Value` (NOT
    /// `Option<String>`) so a newer clank can write a richer
    /// hook value shape (e.g. `{"future_hook": {"cmd": "x",
    /// "env": {...}}}`) and an older clank's read-modify-write
    /// preserves it intact. Codex caught the regression on
    /// 2706484: narrowing to `Option<String>` would have made
    /// `RepoConfigFile` parsing REJECT any non-string/non-null
    /// unknown hook value — breaking the forward-compat the
    /// typed round-trip is supposed to provide. The lossy
    /// `HooksFile` reader (used by `apply_layer`) already
    /// returns `None` for non-string lookups, so the typed
    /// `Config.hooks` semantic doesn't change.
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
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
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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

pub fn load(repo_root: &Path) -> Config {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    load_with_home(repo_root, home.as_deref())
}

/// Home-explicit [`load`] — for in-process callers (query cores,
/// tests) that supply the home dir rather than reading `$HOME`.
/// Plan: dogfood-init-setup-in-tests (Phase B).
pub fn load_with_home(repo_root: &Path, home: Option<&Path>) -> Config {
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
        if let Some(editor_layer) = diff.editor {
            // Field-by-field merge per OQ7. Start from the
            // current effective editor (Some/None) and overlay
            // any fields the new layer explicitly sets. Codex
            // 8cccb87 caught the whole-struct replacement bug
            // that dropped user-scope args/env when repo-scope
            // set only command.
            let mut effective = cfg.diff.editor.clone().unwrap_or_default();
            if let Some(cmd) = editor_layer.command {
                effective.command = Some(cmd);
                present.insert("diff.editor.command".to_string());
            }
            if let Some(args) = editor_layer.args {
                effective.args = args;
            }
            if let Some(env) = editor_layer.env {
                effective.env = env;
            }
            cfg.diff.editor = Some(effective);
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
            // Catalog default is "false"; report the effective
            // default (NOT "null") when unset so `clank config`
            // matches what `clank diff` actually uses (codex
            // d861e85 caught the catalog/get-value mismatch).
            .unwrap_or(false)
            .to_string(),
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
            let cwd = std::env::current_dir()?;
            match crate::git_io::discover_work_dir(&cwd)? {
                Some(root) => dunce::canonicalize(&root)?,
                None => cwd,
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
    fn diff_wait_get_returns_effective_default_when_unset() {
        // Codex d861e85: catalog says default=false; consumer
        // uses unwrap_or(false). `clank config diff.wait get`
        // should report "false", not "null", so the user sees
        // the same value the consumer will use.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_with_home(tmp.path(), None);
        assert_eq!(get_value(&cfg, "diff.wait"), "false");
    }

    #[test]
    fn diff_editor_subfields_merge_per_oq7() {
        // Codex caught on 8cccb87: apply_layer was replacing the
        // whole LaunchConfig instead of merging fields. Repo-scope
        // setting only `command` would drop user-scope `args` and
        // `env`.
        //
        // OQ7 pinned: field-by-field layering. User-scope sets
        // editor.command + editor.args + editor.env; repo-scope
        // sets only editor.command. Result: command overridden;
        // args + env preserved from user-scope.
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".clank/config.json"),
            r#"{
                "diff": {
                    "editor": {
                        "command": "vim",
                        "args": ["-no-plugin", "-N"],
                        "env": {"VIMRUNTIME": "/usr/share/vim"}
                    }
                }
            }"#,
        );
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{"diff": {"editor": {"command": "emacsclient"}}}"#,
        );
        let cfg = load_with_home(tmp.path(), Some(home.path()));
        let editor = cfg.diff.editor.expect("editor present");
        // Command overridden by repo-scope.
        assert_eq!(editor.command.as_deref(), Some("emacsclient"));
        // Args + env preserved from user-scope (repo-scope didn't
        // set them).
        assert_eq!(
            editor.args,
            vec!["-no-plugin".to_string(), "-N".to_string()],
            "user-scope args must survive when repo-scope didn't set them"
        );
        assert_eq!(
            editor.env.get("VIMRUNTIME").map(|s| s.as_str()),
            Some("/usr/share/vim"),
            "user-scope env must survive when repo-scope didn't set them"
        );
    }

    #[test]
    fn diff_editor_subfields_repo_args_replaces_user_args() {
        // Field-by-field doesn't deep-merge lists/maps — when
        // repo-scope DOES set args, it REPLACES the whole list.
        // (Same as how `--launch-arg` works on `clank agent add`.)
        // Codex's OQ7 reasoning: each field is independently
        // overridden when present in the layer.
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".clank/config.json"),
            r#"{"diff": {"editor": {"command": "vim", "args": ["-A"]}}}"#,
        );
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{"diff": {"editor": {"args": ["-B", "-C"]}}}"#,
        );
        let cfg = load_with_home(tmp.path(), Some(home.path()));
        let editor = cfg.diff.editor.expect("editor present");
        // Command from user-scope (repo didn't set it).
        assert_eq!(editor.command.as_deref(), Some("vim"));
        // Args fully replaced by repo-scope.
        assert_eq!(editor.args, vec!["-B".to_string(), "-C".to_string()]);
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
    fn hooks_section_round_trips_through_apply_layer() {
        // typed-config-dogfood Phase 1 acceptance gate.
        // Writes via the typed RepoConfigFile + HooksSection path,
        // reads via apply_layer (the lossy loader). The two paths
        // MUST produce equivalent in-memory Config.hooks results
        // — without this test, the round-trip and lossy paths
        // could drift silently and most consumers go through
        // apply_layer.
        let tmp = tempfile::tempdir().unwrap();
        let file = RepoConfigFile {
            hooks: Some(HooksSection {
                master_work: Some(Some("echo hello".into())),
                idle: Some(None), // explicitly disabled
                ..Default::default()
            }),
            ..Default::default()
        };
        let path = tmp.path().join(".clank/config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
        let cfg = load_with_home(tmp.path(), None);
        assert_eq!(
            cfg.hooks.get(&HookEvent::MasterWork),
            Some(&Some("echo hello".to_string())),
            "master_work hook should round-trip; cfg.hooks={:?}",
            cfg.hooks
        );
        assert_eq!(
            cfg.hooks.get(&HookEvent::Idle),
            Some(&None),
            "explicit null should round-trip as Some(None) (disable)"
        );
        assert!(
            !cfg.hooks.contains_key(&HookEvent::ReviewerWork),
            "unset hook must not appear in the loaded config"
        );
    }

    #[test]
    fn hooks_section_forward_compat_unknown_event_preserved() {
        // Ruthless review of b6323be: HooksSection's `extra`
        // catchall preserves unknown hook event names on
        // round-trip. Without the per-section `extra`, serde
        // would silently drop them.
        let raw = r#"{
            "hooks": {
                "master_work": "echo m",
                "some_future_event": "echo future"
            }
        }"#;
        let parsed: RepoConfigFile = serde_json::from_str(raw).unwrap();
        let hooks = parsed.hooks.as_ref().expect("hooks section present");
        assert_eq!(hooks.master_work, Some(Some("echo m".to_string())));
        assert_eq!(
            hooks.extra.get("some_future_event"),
            Some(&serde_json::Value::String("echo future".to_string())),
            "unknown hook event must land in hooks.extra; got: {:?}",
            hooks.extra
        );
        // Re-serialize and assert the unknown key survives the
        // round trip.
        let out = serde_json::to_string(&parsed).unwrap();
        assert!(
            out.contains("some_future_event"),
            "forward-compat hook key must survive serialize; got: {out}"
        );
    }

    #[test]
    fn hooks_section_forward_compat_preserves_non_string_unknown_hook_value() {
        // Codex caught on 2706484: HooksSection.extra was typed
        // as BTreeMap<String, Option<String>>, so any unknown
        // hook whose value was NOT a string/null would make
        // serde refuse to parse the file. A newer clank that
        // writes a richer hook shape (object, array, number)
        // would have broken an older clank's read-modify-write.
        //
        // Fix: extra is BTreeMap<String, serde_json::Value> so
        // any JSON value round-trips intact.
        let raw = r#"{
            "hooks": {
                "master_work": "echo m",
                "future_object_hook": {"cmd": "x", "args": ["a", "b"]},
                "future_array_hook": ["echo", "first", "echo", "second"],
                "future_number_hook": 42
            }
        }"#;
        let parsed: RepoConfigFile = serde_json::from_str(raw).expect(
            "non-string unknown hook values must NOT cause deserialize failure — \
             that would break forward-compat",
        );
        let hooks = parsed.hooks.as_ref().expect("hooks section present");
        // Known typed field still parsed normally.
        assert_eq!(hooks.master_work, Some(Some("echo m".to_string())));
        // Non-string values preserved as raw Value.
        assert!(hooks.extra.get("future_object_hook").unwrap().is_object());
        assert!(hooks.extra.get("future_array_hook").unwrap().is_array());
        assert_eq!(
            hooks.extra.get("future_number_hook").unwrap().as_i64(),
            Some(42)
        );
        // Round-trip preserves them all.
        let out = serde_json::to_string(&parsed).unwrap();
        assert!(out.contains("future_object_hook"));
        assert!(out.contains("future_array_hook"));
        assert!(out.contains("future_number_hook"));
        assert!(out.contains("\"cmd\":\"x\""));
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
        // and preserves the values.
        let legacy_json = r#"{
            "review": {
                "force_review_on_misc_commits": true,
                "force_review_on_plan_commits": true
            }
        }"#;
        let parsed: RepoConfigFile = serde_json::from_str(legacy_json).unwrap();
        let review = parsed.review.as_ref().expect("review section present");
        assert_eq!(review.adhoc_feedback, Some(true));
        assert_eq!(review.plan_feedback, Some(true));
        // Reserialize and confirm.
        let out = serde_json::to_string(&parsed).unwrap();
        assert!(out.contains(r#""adhoc_feedback":true"#));
        assert!(out.contains(r#""plan_feedback":true"#));
        assert!(!out.contains("force_review_on"));
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
