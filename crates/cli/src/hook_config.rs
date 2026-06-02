use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use clank_core::HookEvent;
use clank_core::ids::{CommitSha, PlanKey};
use clank_core::vocab::CommitGateState;

pub type HookConfig = BTreeMap<HookEvent, Option<String>>;

pub struct HookFiring {
    pub event: HookEvent,
    pub plan: PlanKey,
    pub sha: CommitSha,
    pub gate: Option<CommitGateState>,
    pub next: Option<String>,
}

pub fn run_hook(repo: &Path, config: &HookConfig, firing: &HookFiring) {
    let cmd = match config.get(&firing.event) {
        Some(Some(c)) => c,
        _ => return,
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
    let cmd = config.get(&HookEvent::Idle)?.as_ref()?;
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
