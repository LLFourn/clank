//! `clank doctor` — diagnose whether the clank integration is
//! correctly set up across all three scopes (repo, user-wide,
//! current session). Reads the same loaders the rest of the CLI
//! uses so it catches drift between on-disk state and what the
//! resolver / stop-hook will see.
//!
//! Exit code: 0 if all checks are OK/Warn; 1 if any are Fail.

use std::path::{Path, PathBuf};

use super::DoctorArgs;
use crate::agent_env::{detect_session_from_env, explicit_label_from_env};
use crate::agent_store::{
    agent_config_path, agents_root, load_all_agent_configs, load_repo_config,
};
use clank_core::role_for;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub section: &'static str,
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

impl CheckResult {
    fn ok(section: &'static str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Ok,
            message: message.into(),
        }
    }
    fn warn(section: &'static str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Warn,
            message: message.into(),
        }
    }
    fn fail(section: &'static str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Fail,
            message: message.into(),
        }
    }
}

pub async fn run(args: DoctorArgs) -> anyhow::Result<()> {
    let mut results: Vec<CheckResult> = Vec::new();

    // Repo scope — skip if we're not in a clank-initialized repo.
    let repo = super::resolve_repo(args.repo.as_deref()).ok();
    if let Some(repo) = repo.as_deref() {
        results.extend(repo_checks(repo));
    } else {
        results.push(CheckResult::warn(
            "repo",
            "repo",
            "not inside a git repo (skipping repo checks)",
        ));
    }

    // User scope — independent of repo.
    results.extend(user_checks());

    // Session scope — only meaningful inside an agent.
    results.extend(session_checks(repo.as_deref()));

    render(&results, args.json)?;

    if results
        .iter()
        .any(|r| matches!(r.status, CheckStatus::Fail))
    {
        std::process::exit(1);
    }
    Ok(())
}

fn repo_checks(repo: &Path) -> Vec<CheckResult> {
    let mut out: Vec<CheckResult> = Vec::new();
    const SECTION: &str = "repo";

    // .clank/.gitignore presence.
    let gi = repo.join(".clank/.gitignore");
    match std::fs::read_to_string(&gi) {
        Ok(_) => out.push(CheckResult::ok(
            SECTION,
            ".clank/.gitignore",
            format!("present at {}", gi.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.push(CheckResult::warn(
            SECTION,
            ".clank/.gitignore",
            "missing — run `clank init`".to_string(),
        )),
        Err(e) => out.push(CheckResult::fail(
            SECTION,
            ".clank/.gitignore",
            format!("read failed: {e}"),
        )),
    }

    // Root-gitignore carve-outs (via git check-ignore probes).
    out.push(check_gitignore_probe(repo, ".clank/config.json", true));
    out.push(check_gitignore_probe(
        repo,
        ".clank/agents/some/feedback/foo/abc.md",
        true,
    ));

    // .claude/settings.local.json permissions.
    out.push(check_claude_perms(repo));

    // Per-agent configs.
    if agents_root(repo).is_dir() {
        match load_all_agent_configs(repo) {
            Ok(agents) => {
                if agents.is_empty() {
                    out.push(CheckResult::ok(
                        SECTION,
                        "agents",
                        "no agents bound yet (run `clank as <label>` in an agent session)"
                            .to_string(),
                    ));
                }
                for (label, cfg) in &agents {
                    out.push(CheckResult::ok(
                        SECTION,
                        format!("agent: {}", label.as_str()),
                        format!(
                            "{}: auto_mode={}, session={}",
                            agent_config_path(repo, label).display(),
                            cfg.auto_mode.as_str(),
                            cfg.session
                                .as_ref()
                                .map(|s| format!(
                                    "{} bound to {} ({})",
                                    s.tool.as_str(),
                                    s.id.as_str(),
                                    s.updated_at
                                ))
                                .unwrap_or_else(|| "unbound".to_string()),
                        ),
                    ));
                }
            }
            Err(e) => out.push(CheckResult::fail(
                SECTION,
                "agents",
                format!("failed to load agent configs: {e:#}"),
            )),
        }
    } else {
        out.push(CheckResult::ok(
            SECTION,
            "agents",
            ".clank/agents/ does not exist yet (created lazily)".to_string(),
        ));
    }

    // Repo-level config (master designation).
    match load_repo_config(repo) {
        Ok(Some(cfg)) => {
            if let Some(master) = &cfg.master {
                let master_dir = agents_root(repo).join(master.as_str());
                if master_dir.is_dir() {
                    out.push(CheckResult::ok(
                        SECTION,
                        ".clank/config.json",
                        format!("master = `{}`", master.as_str()),
                    ));
                } else {
                    out.push(CheckResult::warn(
                        SECTION,
                        ".clank/config.json",
                        format!(
                            "master = `{}` but no `.clank/agents/{}/` dir exists locally \
                             (harmless if that agent has never run here)",
                            master.as_str(),
                            master.as_str(),
                        ),
                    ));
                }
            } else {
                out.push(CheckResult::ok(
                    SECTION,
                    ".clank/config.json",
                    "present, no master designated".to_string(),
                ));
            }
        }
        Ok(None) => out.push(CheckResult::ok(
            SECTION,
            ".clank/config.json",
            "not present (no master designated; optional)".to_string(),
        )),
        Err(e) => out.push(CheckResult::fail(
            SECTION,
            ".clank/config.json",
            format!("parse failed: {e:#}"),
        )),
    }

    out
}

/// Probe whether git considers `rel` ignored. If `expect_tracked`,
/// "ignored" is a Warn (means the root gitignore lacks a needed
/// carve-out). The probed path doesn't need to exist on disk —
/// git matches patterns, not files.
fn check_gitignore_probe(repo: &Path, rel: &str, expect_tracked: bool) -> CheckResult {
    const SECTION: &str = "repo";
    let probe = repo.join(rel);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-v"])
        .arg(probe.as_path())
        .output();
    let Ok(output) = output else {
        return CheckResult::warn(
            SECTION,
            format!("gitignore probe: {rel}"),
            "git check-ignore failed to run".to_string(),
        );
    };
    let ignored = output.status.code() == Some(0);
    if expect_tracked && ignored {
        let line = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim_end()
            .to_string();
        CheckResult::warn(
            SECTION,
            format!("gitignore probe: {rel}"),
            format!(
                "expected TRACKED but git reports ignored. Add the recommended \
                 carve-out to the root .gitignore. Source: {line}"
            ),
        )
    } else {
        CheckResult::ok(
            SECTION,
            format!("gitignore probe: {rel}"),
            format!("tracked (as expected)"),
        )
    }
}

fn check_claude_perms(repo: &Path) -> CheckResult {
    const SECTION: &str = "repo";
    const NAME: &str = ".claude/settings.local.json";
    const REQUIRED: &[&str] = &[
        "Write(.clank/agents/**)",
        "Edit(.clank/agents/**)",
        "Read(.clank/agents/**)",
    ];
    let path = repo.join(".claude/settings.local.json");
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::warn(
                SECTION,
                NAME,
                "missing — run `clank init` to write the agent edit-permission rules".to_string(),
            );
        }
        Err(e) => return CheckResult::fail(SECTION, NAME, format!("read failed: {e}")),
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return CheckResult::fail(SECTION, NAME, format!("parse failed: {e}")),
    };
    let allow = match v
        .get("permissions")
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
    {
        Some(a) => a,
        None => {
            return CheckResult::warn(
                SECTION,
                NAME,
                "missing `permissions.allow` array — run `clank init`".to_string(),
            );
        }
    };
    let entries: Vec<&str> = allow.iter().filter_map(|x| x.as_str()).collect();
    let missing: Vec<&&str> = REQUIRED.iter().filter(|r| !entries.contains(r)).collect();
    if missing.is_empty() {
        CheckResult::ok(
            SECTION,
            NAME,
            "all three agent edit-permission rules present".to_string(),
        )
    } else {
        let names: Vec<String> = missing.iter().map(|s| (**s).to_string()).collect();
        CheckResult::warn(
            SECTION,
            NAME,
            format!(
                "missing rules: {} — run `clank init` to add",
                names.join(", ")
            ),
        )
    }
}

fn user_checks() -> Vec<CheckResult> {
    const SECTION: &str = "user";
    let mut out = Vec::<CheckResult>::new();

    let home = match std::env::var_os("HOME").map(PathBuf::from) {
        Some(h) => h,
        None => {
            out.push(CheckResult::fail(
                SECTION,
                "home",
                "HOME env var unset — can't locate ~/.claude or ~/.codex".to_string(),
            ));
            return out;
        }
    };

    // Skill files.
    out.push(check_skill_file(
        &home.join(".claude/skills/clank/SKILL.md"),
        crate::cli::setup::CLAUDE_SKILL_BODY,
        "~/.claude/skills/clank/SKILL.md",
    ));
    out.push(check_skill_file(
        &home.join(".codex/skills/clank/SKILL.md"),
        crate::cli::setup::CODEX_SKILL_BODY,
        "~/.codex/skills/clank/SKILL.md",
    ));
    out.push(check_skill_file(
        &home.join(".codex/commands/clank.md"),
        crate::cli::setup::CODEX_COMMAND_BODY,
        "~/.codex/commands/clank.md",
    ));

    // Hook entries.
    out.push(check_hook_entry(
        &home.join(".claude/settings.json"),
        "claude",
        "~/.claude/settings.json",
    ));
    out.push(check_hook_entry(
        &home.join(".codex/hooks.json"),
        "codex",
        "~/.codex/hooks.json",
    ));

    out
}

fn check_skill_file(path: &Path, expected: &str, display: &str) -> CheckResult {
    const SECTION: &str = "user";
    match std::fs::read_to_string(path) {
        Ok(s) if s == expected => CheckResult::ok(SECTION, display, "matches embedded content"),
        Ok(_) => CheckResult::warn(
            SECTION,
            display,
            "drifted from embedded content — run `clank setup --force` to refresh".to_string(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            CheckResult::warn(SECTION, display, "missing — run `clank setup`".to_string())
        }
        Err(e) => CheckResult::fail(SECTION, display, format!("read failed: {e}")),
    }
}

fn check_hook_entry(path: &Path, tool: &str, display: &str) -> CheckResult {
    const SECTION: &str = "user";
    let body = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::warn(SECTION, display, "missing — run `clank setup`".to_string());
        }
        Err(e) => return CheckResult::fail(SECTION, display, format!("read failed: {e}")),
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return CheckResult::fail(SECTION, display, format!("parse failed: {e}")),
    };
    let stop = match v
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(|s| s.as_array())
    {
        Some(s) => s,
        None => {
            return CheckResult::warn(
                SECTION,
                display,
                "no Stop hooks configured — run `clank setup`".to_string(),
            );
        }
    };
    let has_clank = stop.iter().any(|wrapper| {
        let Some(inner) = wrapper.get("hooks").and_then(|h| h.as_array()) else {
            return false;
        };
        inner.iter().any(|h| {
            h.get("id").and_then(|v| v.as_str()) == Some("clank-stop-hook")
                || h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|cmd| cmd.starts_with("clank stop-hook"))
        })
    });
    if has_clank {
        CheckResult::ok(SECTION, display, format!("{tool} Stop hook installed"))
    } else {
        CheckResult::warn(
            SECTION,
            display,
            format!("no clank Stop hook in {tool} config — run `clank setup`"),
        )
    }
}

fn session_checks(repo: Option<&Path>) -> Vec<CheckResult> {
    const SECTION: &str = "session";
    let mut out = Vec::<CheckResult>::new();

    let detected = match detect_session_from_env() {
        Ok(d) => d,
        Err(e) => {
            out.push(CheckResult::fail(
                SECTION,
                "env",
                format!("env detection failed: {e}"),
            ));
            return out;
        }
    };
    let Some((tool, session_id)) = detected else {
        out.push(CheckResult::ok(
            SECTION,
            "env",
            "not running inside an agent (no CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID)".to_string(),
        ));
        return out;
    };
    out.push(CheckResult::ok(
        SECTION,
        "env",
        format!(
            "running inside {} (session {})",
            tool.as_str(),
            session_id.as_str()
        ),
    ));

    // CLANK_AGENT override?
    match explicit_label_from_env() {
        Ok(Some(label)) => out.push(CheckResult::ok(
            SECTION,
            "CLANK_AGENT",
            format!("explicit override active: `{}`", label.as_str()),
        )),
        Ok(None) => {}
        Err(e) => out.push(CheckResult::warn(
            SECTION,
            "CLANK_AGENT",
            format!("invalid override: {e}"),
        )),
    }

    let Some(repo) = repo else {
        out.push(CheckResult::warn(
            SECTION,
            "identity",
            "not in a clank repo — can't resolve identity".to_string(),
        ));
        return out;
    };

    // Find which agent (if any) has this session bound.
    let agents = match load_all_agent_configs(repo) {
        Ok(a) => a,
        Err(e) => {
            out.push(CheckResult::fail(
                SECTION,
                "identity",
                format!("loading agent configs failed: {e:#}"),
            ));
            return out;
        }
    };
    let bound = agents.iter().find(|(_, cfg)| {
        cfg.session
            .as_ref()
            .is_some_and(|s| s.id == session_id && s.tool == tool)
    });
    match bound {
        Some((label, cfg)) => {
            let updated_at = cfg
                .session
                .as_ref()
                .map(|s| s.updated_at.clone())
                .unwrap_or_default();
            out.push(CheckResult::ok(
                SECTION,
                "identity",
                format!(
                    "this session is bound to agent `{}` (last bound {})",
                    label.as_str(),
                    updated_at
                ),
            ));
            // Inferred role.
            let repo_cfg = load_repo_config(repo).ok().flatten();
            let role = role_for(label, repo_cfg.as_ref());
            out.push(CheckResult::ok(
                SECTION,
                "role",
                format!("inferred role: {}", role.as_str()),
            ));
        }
        None => out.push(CheckResult::fail(
            SECTION,
            "identity",
            format!(
                "no agent is bound to this session — run `clank as <label>` \
                 (inside this session) or `clank init` to bootstrap"
            ),
        )),
    }

    out
}

fn render(results: &[CheckResult], json: bool) -> anyhow::Result<()> {
    if json {
        let payload: Vec<_> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "section": r.section,
                    "name": r.name,
                    "status": match r.status {
                        CheckStatus::Ok => "ok",
                        CheckStatus::Warn => "warn",
                        CheckStatus::Fail => "fail",
                    },
                    "message": r.message,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&payload)?);
        return Ok(());
    }

    let mut current_section: Option<&str> = None;
    for r in results {
        if Some(r.section) != current_section {
            println!("\n[{}]", r.section);
            current_section = Some(r.section);
        }
        let tag = match r.status {
            CheckStatus::Ok => "OK   ",
            CheckStatus::Warn => "WARN ",
            CheckStatus::Fail => "FAIL ",
        };
        println!("  {tag}{}: {}", r.name, r.message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git_repo() -> tempfile::TempDir {
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
    fn repo_checks_warn_on_missing_gitignore() {
        let dir = init_git_repo();
        let results = repo_checks(dir.path());
        let gi = results
            .iter()
            .find(|r| r.name == ".clank/.gitignore")
            .expect("gitignore check ran");
        assert_eq!(gi.status, CheckStatus::Warn);
        assert!(gi.message.contains("clank init"));
    }

    #[test]
    fn repo_checks_warn_on_missing_claude_perms() {
        let dir = init_git_repo();
        let results = repo_checks(dir.path());
        let perm = results
            .iter()
            .find(|r| r.name == ".claude/settings.local.json")
            .expect("perms check ran");
        assert_eq!(perm.status, CheckStatus::Warn);
        assert!(perm.message.contains("clank init"));
    }

    #[test]
    fn check_skill_file_ok_on_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SKILL.md");
        std::fs::write(&p, "expected").unwrap();
        let r = check_skill_file(&p, "expected", "test");
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[test]
    fn check_skill_file_warn_on_drift() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SKILL.md");
        std::fs::write(&p, "drifted").unwrap();
        let r = check_skill_file(&p, "expected", "test");
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.message.contains("--force"));
    }

    #[test]
    fn check_skill_file_warn_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let r = check_skill_file(&dir.path().join("nope.md"), "x", "test");
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.message.contains("clank setup"));
    }

    #[test]
    fn check_hook_entry_ok_when_id_present() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"Stop":[
                {"hooks":[{"id":"clank-stop-hook","type":"command","command":"clank stop-hook --tool claude"}]}
            ]}}"#,
        )
        .unwrap();
        let r = check_hook_entry(&p, "claude", "test");
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[test]
    fn check_hook_entry_warn_when_missing_clank_hook() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"Stop":[
                {"hooks":[{"type":"command","command":"/other/hook"}]}
            ]}}"#,
        )
        .unwrap();
        let r = check_hook_entry(&p, "claude", "test");
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn check_hook_entry_warn_when_no_stop_array() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, r#"{"hooks":{}}"#).unwrap();
        let r = check_hook_entry(&p, "claude", "test");
        assert_eq!(r.status, CheckStatus::Warn);
    }
}
