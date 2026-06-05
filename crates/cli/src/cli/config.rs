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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            review: ReviewConfig::default(),
            hooks: BTreeMap::new(),
        }
    }
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
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct RepoAgentsFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<DefaultAgent>,
}

/// User-scope `~/.clank/config.json` deserialization wrapper for
/// the `default_agents` field. Public for the same reason as
/// [`RepoAgentsFile`].
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct UserAgentsFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_agents: Vec<DefaultAgent>,
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
    Ok(parsed.default_agents)
}

/// Strict loader for the repo-scope `agents` list at
/// `<repo>/.clank/config.json`. Same failure-policy semantics as
/// [`load_default_agents`] (missing = empty; malformed = error)
/// for the same reason: silently dropping a multi-reviewer set
/// would convert the repo to auto-finalize.
pub fn load_repo_agents(repo_root: &Path) -> anyhow::Result<Vec<DefaultAgent>> {
    let path = repo_root.join(".clank/config.json");
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(e))
                .with_context(|| format!("reading repo config {}", path.display()));
        }
    };
    let parsed: RepoAgentsFile = serde_json::from_str(&body)
        .with_context(|| format!("parsing repo config {}", path.display()))?;
    Ok(parsed.agents)
}

/// Merged agent declaration: repo-scope `agents` if present (REPLACE
/// semantics — `the local project can modify it`); else fall back
/// to user-scope `default_agents`. This is the source of truth for
/// "which agents exist + their role + tool + launch profile" per
/// `agent-add-cli-and-repo-scope`.
pub fn load_merged_agents(
    repo_root: &Path,
    home: Option<&Path>,
) -> anyhow::Result<Vec<DefaultAgent>> {
    let repo = load_repo_agents(repo_root)?;
    if !repo.is_empty() {
        return Ok(repo);
    }
    load_default_agents(home)
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

fn key_to_json_path(key: &str) -> Option<(&'static str, &'static str)> {
    match key {
        "review.adhoc_feedback" => Some(("review", "adhoc_feedback")),
        "review.plan_feedback" => Some(("review", "plan_feedback")),
        "review.require_commit_prefix" => Some(("review", "require_commit_prefix")),
        "hooks.master_work" => Some(("hooks", "master_work")),
        "hooks.reviewer_work" => Some(("hooks", "reviewer_work")),
        "hooks.plan_finalized" => Some(("hooks", "plan_finalized")),
        "hooks.idle" => Some(("hooks", "idle")),
        "hooks.blocked" => Some(("hooks", "blocked")),
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
        | ConfigKey::HooksBlocked(a) => a,
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
            let (section, field) =
                key_to_json_path(key).expect("key_name always returns a catalog key");
            set_repo_key(&repo, section, field, value, def.type_desc)?;
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

fn set_repo_key(
    repo: &Path,
    section: &str,
    field: &str,
    value: &str,
    type_desc: &str,
) -> anyhow::Result<()> {
    let config_path = repo.join(".clank/config.json");
    let mut root: serde_json::Value = if config_path.exists() {
        let body = std::fs::read_to_string(&config_path)?;
        serde_json::from_str(&body)?
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config.json is not a JSON object"))?;

    let sec = obj
        .entry(section)
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    let sec_obj = sec
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("config section `{section}` is not an object"))?;

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

    sec_obj.insert(field.to_string(), json_val);

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
        set_repo_key(tmp.path(), "review", "adhoc_feedback", "false", "bool").unwrap();
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
        set_repo_key(tmp.path(), "review", "adhoc_feedback", "false", "bool").unwrap();
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
        set_repo_key(tmp.path(), "hooks", "master_work", "true", "string|null").unwrap();
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
        set_repo_key(tmp.path(), "hooks", "idle", "null", "string|null").unwrap();
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
