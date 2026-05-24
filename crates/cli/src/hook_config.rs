//! Hook configuration loading and execution for lifecycle hooks.
//!
//! Two config files, merged (repo overlays user defaults):
//! 1. `~/.clank/hooks.json` (user-level defaults)
//! 2. `<repo>/.clank/hooks.json` (repo-level overrides per-event)

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use clank_core::HookEvent;
use clank_core::ids::{CommitSha, PlanKey};

pub type HookConfig = BTreeMap<HookEvent, String>;

pub struct HookFiring {
    pub event: HookEvent,
    pub plan: PlanKey,
    pub sha: CommitSha,
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
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("CLANK_EVENT", firing.event.as_str())
        .env("CLANK_PLAN", firing.plan.as_str())
        .env("CLANK_REPO", &*repo.to_string_lossy())
        .env("CLANK_SHA", firing.sha.as_str())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_when_no_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_hook_config(dir.path());
        assert!(config.is_empty());
    }

    #[test]
    fn merge_repo_overrides_user_per_event() {
        let mut config = HookConfig::new();
        let dir = tempfile::tempdir().unwrap();

        let user = dir.path().join("user.json");
        std::fs::write(
            &user,
            r#"{"plan-introduced":"user-cmd","review-received":"user-review"}"#,
        )
        .unwrap();
        merge_from_file(&mut config, &user);
        assert_eq!(config[&HookEvent::PlanIntroduced], "user-cmd");
        assert_eq!(config[&HookEvent::ReviewReceived], "user-review");

        let repo = dir.path().join("repo.json");
        std::fs::write(&repo, r#"{"plan-introduced":"repo-cmd"}"#).unwrap();
        merge_from_file(&mut config, &repo);
        assert_eq!(config[&HookEvent::PlanIntroduced], "repo-cmd");
        assert_eq!(config[&HookEvent::ReviewReceived], "user-review");
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
