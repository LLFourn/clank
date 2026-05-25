use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clank_core::HookEvent;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub review: ReviewConfig,
    pub hooks: BTreeMap<HookEvent, String>,
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
            adhoc_feedback: true,
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

#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default, alias = "master-work")]
    master_work: Option<String>,
    #[serde(default, alias = "reviewer-work")]
    reviewer_work: Option<String>,
    #[serde(default, alias = "plan-finalized")]
    plan_finalized: Option<String>,
    #[serde(default)]
    idle: Option<String>,
}

pub fn load(repo_root: &Path) -> Config {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    load_with_home(repo_root, home.as_deref())
}

fn load_with_home(repo_root: &Path, home: Option<&Path>) -> Config {
    let mut cfg = Config::default();
    apply_layer(&mut cfg, home.map(|h| h.join(".clank/config.json")).as_deref());
    apply_layer(&mut cfg, Some(&repo_root.join(".clank/config.json")));

    // Merge legacy hooks.json files (repo overrides user) then apply as
    // fallback so config.json always wins.
    let mut legacy: BTreeMap<HookEvent, String> = BTreeMap::new();
    if let Some(user_path) = home.map(|h| h.join(".clank/hooks.json")) {
        merge_legacy_file(&mut legacy, &user_path);
    }
    // repo overwrites user
    merge_legacy_file(&mut legacy, &repo_root.join(".clank/hooks.json"));
    for (event, cmd) in legacy {
        cfg.hooks.entry(event).or_insert(cmd);
    }

    cfg
}

fn apply_layer(cfg: &mut Config, path: Option<&Path>) {
    let Some(path) = path else { return };
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "clank config: read failed; ignoring layer");
            return;
        }
    };
    let parsed: ConfigFile = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "clank config: malformed JSON; ignoring layer");
            return;
        }
    };
    if let Some(review) = parsed.review {
        if let Some(v) = review.adhoc_feedback {
            cfg.review.adhoc_feedback = v;
        }
        if let Some(v) = review.plan_feedback {
            cfg.review.plan_feedback = v;
        }
        if let Some(v) = review.require_commit_prefix {
            cfg.review.require_commit_prefix = v;
        }
    }
    if let Some(hooks) = parsed.hooks {
        if let Some(v) = hooks.master_work {
            cfg.hooks.insert(HookEvent::MasterWork, v);
        }
        if let Some(v) = hooks.reviewer_work {
            cfg.hooks.insert(HookEvent::ReviewerWork, v);
        }
        if let Some(v) = hooks.plan_finalized {
            cfg.hooks.insert(HookEvent::PlanFinalized, v);
        }
        if let Some(v) = hooks.idle {
            cfg.hooks.insert(HookEvent::Idle, v);
        }
    }
}

fn merge_legacy_file(out: &mut BTreeMap<HookEvent, String>, path: &Path) {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "clank hooks.json: read failed; ignoring");
            return;
        }
    };
    let parsed: BTreeMap<HookEvent, String> = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "clank hooks.json: malformed JSON; ignoring");
            return;
        }
    };
    tracing::warn!(
        path = %path.display(),
        "hooks.json is deprecated; move hooks into the `hooks` section of config.json"
    );
    for (event, cmd) in parsed {
        out.insert(event, cmd);
    }
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
        default: "true",
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
];

// ── Source tracking ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueSource {
    Default,
    User,
    Repo,
    Legacy,
}

impl std::fmt::Display for ValueSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueSource::Default => f.write_str("default"),
            ValueSource::User => f.write_str("user"),
            ValueSource::Repo => f.write_str("repo"),
            ValueSource::Legacy => f.write_str("legacy"),
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
    let default_cfg = Config::default();
    let mut sources: BTreeMap<String, ValueSource> = BTreeMap::new();

    let mut user_cfg = Config::default();
    apply_layer(
        &mut user_cfg,
        home.map(|h| h.join(".clank/config.json")).as_deref(),
    );

    let mut repo_cfg = user_cfg.clone();
    apply_layer(&mut repo_cfg, Some(&repo_root.join(".clank/config.json")));

    // Legacy hooks.json: user first, repo overwrites, then apply as fallback.
    let mut legacy: BTreeMap<HookEvent, String> = BTreeMap::new();
    if let Some(user_hooks) = home.map(|h| h.join(".clank/hooks.json")) {
        if let Ok(body) = std::fs::read_to_string(&user_hooks) {
            if let Ok(m) = serde_json::from_str::<BTreeMap<HookEvent, String>>(&body) {
                legacy.extend(m);
            }
        }
    }
    if let Ok(body) = std::fs::read_to_string(repo_root.join(".clank/hooks.json")) {
        if let Ok(m) = serde_json::from_str::<BTreeMap<HookEvent, String>>(&body) {
            legacy.extend(m);
        }
    }

    let mut full_cfg = repo_cfg.clone();
    for (event, cmd) in &legacy {
        full_cfg.hooks.entry(*event).or_insert_with(|| cmd.clone());
    }

    macro_rules! track_bool {
        ($field:expr, $key:expr) => {
            let key = $key.to_string();
            if $field(&repo_cfg) != $field(&default_cfg) {
                sources.insert(key, ValueSource::Repo);
            } else if $field(&user_cfg) != $field(&default_cfg) {
                sources.insert(key, ValueSource::User);
            }
        };
    }

    track_bool!(|c: &Config| c.review.adhoc_feedback, "review.adhoc_feedback");
    track_bool!(|c: &Config| c.review.plan_feedback, "review.plan_feedback");
    track_bool!(
        |c: &Config| c.review.require_commit_prefix,
        "review.require_commit_prefix"
    );

    for event in [
        HookEvent::MasterWork,
        HookEvent::ReviewerWork,
        HookEvent::PlanFinalized,
        HookEvent::Idle,
    ] {
        let key = format!("hooks.{}", event_to_key_name(event));
        let in_repo_cfg = repo_cfg.hooks.contains_key(&event);
        let in_user_cfg = user_cfg.hooks.contains_key(&event);
        let in_legacy = legacy.contains_key(&event);
        if in_repo_cfg {
            sources.insert(key, ValueSource::Repo);
        } else if in_user_cfg {
            sources.insert(key, ValueSource::User);
        } else if in_legacy {
            sources.insert(key, ValueSource::Legacy);
        }
    }

    KEY_CATALOG
        .iter()
        .map(|def| {
            let key = format!("{}.{}", def.section, def.name);
            let value = get_value(&full_cfg, &key);
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

pub fn get_value(cfg: &Config, key: &str) -> String {
    match key {
        "review.adhoc_feedback" => cfg.review.adhoc_feedback.to_string(),
        "review.plan_feedback" => cfg.review.plan_feedback.to_string(),
        "review.require_commit_prefix" => cfg.review.require_commit_prefix.to_string(),
        "hooks.master_work" => cfg
            .hooks
            .get(&HookEvent::MasterWork)
            .cloned()
            .unwrap_or_else(|| "null".to_string()),
        "hooks.reviewer_work" => cfg
            .hooks
            .get(&HookEvent::ReviewerWork)
            .cloned()
            .unwrap_or_else(|| "null".to_string()),
        "hooks.plan_finalized" => cfg
            .hooks
            .get(&HookEvent::PlanFinalized)
            .cloned()
            .unwrap_or_else(|| "null".to_string()),
        "hooks.idle" => cfg
            .hooks
            .get(&HookEvent::Idle)
            .cloned()
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
        _ => None,
    }
}

// ── run() ─────────────────────────────────────────────────────────────────────

use super::ConfigArgs;

pub async fn run(args: ConfigArgs) -> anyhow::Result<()> {
    let repo = match &args.repo {
        Some(p) => dunce::canonicalize(p)?,
        None => {
            // For `clank config` without --repo, try git toplevel but don't fail.
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

    match (&args.key, &args.action) {
        (None, _) => {
            // No key: dump all
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
        }
        (Some(key), None) => {
            // Key only: show help info for that key
            let def = KEY_CATALOG
                .iter()
                .find(|d| format!("{}.{}", d.section, d.name) == *key)
                .ok_or_else(|| anyhow::anyhow!("unknown config key: {key}"))?;
            println!("key:     {}.{}", def.section, def.name);
            println!("type:    {}", def.type_desc);
            println!("default: {}", def.default);
            println!("help:    {}", def.help);
        }
        (Some(key), Some(action)) if action == "get" => {
            let cfg = load(&repo);
            println!("{}", get_value(&cfg, key));
        }
        (Some(key), Some(action)) if action == "set" => {
            let value = args
                .value
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("set requires a value"))?;
            let (section, field) =
                key_to_json_path(key).ok_or_else(|| anyhow::anyhow!("unknown config key: {key}"))?;
            set_repo_key(&repo, section, field, value)?;
        }
        (Some(_), Some(action)) => {
            anyhow::bail!("unknown action `{action}`; expected get or set");
        }
    }

    Ok(())
}

fn set_repo_key(repo: &Path, section: &str, field: &str, value: &str) -> anyhow::Result<()> {
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

    // Parse the value: try bool, then null, then keep as string.
    let json_val = if value == "null" {
        serde_json::Value::Null
    } else if value == "true" {
        serde_json::Value::Bool(true)
    } else if value == "false" {
        serde_json::Value::Bool(false)
    } else {
        serde_json::Value::String(value.to_string())
    };

    sec_obj.insert(field.to_string(), json_val);

    // Migrate any legacy hooks.json hooks into config.json on first hooks write.
    if section == "hooks" {
        migrate_legacy_hooks_into_object(repo, obj)?;
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&root)?;
    std::fs::write(&config_path, body)?;
    Ok(())
}

fn migrate_legacy_hooks_into_object(
    repo: &Path,
    root: &mut serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    let hooks_path = repo.join(".clank/hooks.json");
    let body = match std::fs::read_to_string(&hooks_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let legacy: BTreeMap<HookEvent, String> = match serde_json::from_str(&body) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };

    let hooks_sec = root
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let hooks_obj = match hooks_sec.as_object_mut() {
        Some(o) => o,
        None => return Ok(()),
    };

    for (event, cmd) in legacy {
        let field = event_to_key_name(event);
        // Only migrate where not already explicitly set.
        hooks_obj
            .entry(field)
            .or_insert_with(|| serde_json::Value::String(cmd));
    }
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
        assert!(cfg.review.adhoc_feedback);
        assert!(cfg.review.plan_feedback);
        assert!(!cfg.review.require_commit_prefix);
        assert!(cfg.hooks.is_empty());
    }

    #[test]
    fn repo_layer_overrides_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".clank/config.json");
        write(
            &repo_cfg,
            r#"{"review": {"adhoc_feedback": false}}"#,
        );
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
        assert!(cfg.review.adhoc_feedback);
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
        assert_eq!(cfg.hooks[&HookEvent::MasterWork], "notify master");
        assert_eq!(cfg.hooks[&HookEvent::Idle], "do idle");
    }

    #[test]
    fn hooks_kebab_alias_in_config_json() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".clank/config.json");
        write(
            &repo_cfg,
            r#"{"hooks": {"master-work": "kebab-cmd"}}"#,
        );
        let cfg = load_isolated(tmp.path());
        assert_eq!(cfg.hooks[&HookEvent::MasterWork], "kebab-cmd");
    }

    #[test]
    fn config_json_hooks_win_over_legacy_hooks_json() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(".clank/config.json"),
            r#"{"hooks": {"master_work": "config-cmd"}}"#,
        );
        write(
            &tmp.path().join(".clank/hooks.json"),
            r#"{"master-work": "legacy-cmd"}"#,
        );
        let cfg = load_isolated(tmp.path());
        assert_eq!(cfg.hooks[&HookEvent::MasterWork], "config-cmd");
    }

    #[test]
    fn legacy_hooks_json_used_as_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join(".clank/hooks.json"),
            r#"{"master-work": "legacy-cmd"}"#,
        );
        let cfg = load_isolated(tmp.path());
        assert_eq!(cfg.hooks[&HookEvent::MasterWork], "legacy-cmd");
    }

    #[test]
    fn set_repo_key_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".clank")).unwrap();
        set_repo_key(tmp.path(), "review", "adhoc_feedback", "false").unwrap();
        let cfg = load_isolated(tmp.path());
        assert!(!cfg.review.adhoc_feedback);
    }

    #[test]
    fn set_hook_migrates_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".clank")).unwrap();
        write(
            &tmp.path().join(".clank/hooks.json"),
            r#"{"reviewer-work": "old-cmd"}"#,
        );
        set_repo_key(tmp.path(), "hooks", "idle", "new-idle").unwrap();
        let body =
            std::fs::read_to_string(tmp.path().join(".clank/config.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // Both the new key and the migrated legacy key should be present.
        assert_eq!(v["hooks"]["idle"], "new-idle");
        assert_eq!(v["hooks"]["reviewer_work"], "old-cmd");
    }

    #[test]
    fn get_value_returns_null_string_for_unset_hook() {
        let cfg = Config::default();
        assert_eq!(get_value(&cfg, "hooks.master_work"), "null");
    }
}
