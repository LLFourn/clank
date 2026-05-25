//! Hook configuration loading and execution for work-item hooks.
//!
//! Two config files, merged (repo overlays user defaults):
//! 1. `~/.clank/hooks.json` (user-level defaults)
//! 2. `<repo>/.clank/hooks.json` (repo-level overrides per-event)

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use clank_core::HookEvent;
use clank_core::ids::{CommitSha, PlanKey};
use clank_core::vocab::CommitGateState;

pub type HookConfig = BTreeMap<HookEvent, String>;

pub struct HookFiring {
    pub event: HookEvent,
    pub plan: PlanKey,
    pub sha: CommitSha,
    pub gate: Option<CommitGateState>,
    pub next: Option<String>,
}

pub fn load_hook_config(repo: &Path) -> HookConfig {
    let mut config = HookConfig::new();
    if let Some(home) = std::env::var_os("HOME") {
        let user_path = std::path::PathBuf::from(home)
            .join(".clank")
            .join("hooks.json");
        merge_from_file(&mut config, &user_path);
    }
    let repo_path = repo.join(".clank").join("hooks.json");
    merge_from_file(&mut config, &repo_path);
    config
}

fn merge_from_file(config: &mut HookConfig, path: &Path) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let parsed: BTreeMap<HookEvent, String> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "warning: hooks.json at `{}` malformed ({e}); ignoring",
                path.display()
            );
            return;
        }
    };
    for (event, cmd) in parsed {
        config.insert(event, cmd);
    }
}

pub fn run_hook(repo: &Path, config: &HookConfig, firing: &HookFiring) {
    let cmd = match config.get(&firing.event) {
        Some(c) => c,
        None => return,
    };
    let mut child = std::process::Command::new("sh");
    child
        .arg("-c")
        .arg(cmd)
        .env("CLANK_EVENT", firing.event.as_str())
        .env("CLANK_PLAN", firing.plan.as_str())
        .env("CLANK_REPO", &*repo.to_string_lossy())
        .env("CLANK_SHA", firing.sha.as_str())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(gate) = &firing.gate {
        child.env("CLANK_GATE", gate.as_str());
    }
    if let Some(next) = &firing.next {
        child.env("CLANK_NEXT", next.as_str());
    }
    let result = child.status();

    match result {
        Ok(status) if !status.success() => {
            eprintln!(
                "warning: lifecycle hook `{}` for plan `{}` exited {:?}",
                firing.event.as_str(),
                firing.plan.as_str(),
                status.code()
            );
        }
        Err(e) => {
            eprintln!(
                "warning: lifecycle hook `{}` for plan `{}` failed to spawn: {e}",
                firing.event.as_str(),
                firing.plan.as_str()
            );
        }
        _ => {}
    }
}

/// Run the idle hook. Unlike work-item hooks, idle captures stdout —
/// non-empty stdout becomes a synthetic prompt. Returns None if no
/// idle hook is configured or stdout is empty.
pub fn run_idle_hook(repo: &Path, config: &HookConfig) -> Option<String> {
    let cmd = config.get(&HookEvent::Idle)?;
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("CLANK_EVENT", "idle")
        .env("CLANK_REPO", &*repo.to_string_lossy())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .ok()?;
    let prompt = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if prompt.is_empty() {
        None
    } else {
        Some(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_empty() {
        let mut config = HookConfig::new();
        let dir = tempfile::tempdir().unwrap();
        merge_from_file(&mut config, &dir.path().join("nonexistent.json"));
        assert!(config.is_empty());
    }

    #[test]
    fn merge_repo_overrides_user_per_event() {
        let mut config = HookConfig::new();
        let dir = tempfile::tempdir().unwrap();

        let user = dir.path().join("user.json");
        std::fs::write(
            &user,
            r#"{"master-work":"user-cmd","reviewer-work":"user-review"}"#,
        )
        .unwrap();
        merge_from_file(&mut config, &user);
        assert_eq!(config[&HookEvent::MasterWork], "user-cmd");
        assert_eq!(config[&HookEvent::ReviewerWork], "user-review");

        let repo = dir.path().join("repo.json");
        std::fs::write(&repo, r#"{"master-work":"repo-cmd"}"#).unwrap();
        merge_from_file(&mut config, &repo);
        assert_eq!(config[&HookEvent::MasterWork], "repo-cmd");
        assert_eq!(config[&HookEvent::ReviewerWork], "user-review");
    }

    #[test]
    fn malformed_json_returns_empty() {
        let mut config = HookConfig::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "not json").unwrap();
        merge_from_file(&mut config, &path);
        assert!(config.is_empty());
    }
}
