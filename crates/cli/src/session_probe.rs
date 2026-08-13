//! Is a tool's session still resumable?
//!
//! One probe, shared by every caller that is about to hand a session
//! id to a tool. `clank open` and `clank fork` asked the same question
//! and answered it differently — open checked, fork did not — which is
//! how a dangling binding reached `--resume` and launched nothing
//! (fork-stale-binding-launches-fresh).

use std::path::Path;

use clank_core::vocab::Tool;

/// `None` = the tool's store can't be probed yet — never conflated
/// with "session gone" (codex e5d0880).
pub(crate) fn session_jsonl_exists(
    tool: &Tool,
    session_id: &str,
    home: Option<&Path>,
) -> Option<bool> {
    // opencode's store can't be probed until opencode-agent-tool M2:
    // UNKNOWN unconditionally — with or without a HOME, a valid
    // binding must never read as "definitely gone" (codex 5764fcf).
    if matches!(tool, Tool::OpenCode) {
        return None;
    }
    // No HOME keeps the pre-existing "not resumable" answer for the
    // probeable tools.
    let Some(home) = home else {
        return Some(false);
    };
    match tool {
        Tool::Claude => Some(claude_session_jsonl_exists(home, session_id)),
        Tool::Codex => Some(codex_session_jsonl_exists(home, session_id)),
        Tool::Grok => Some(grok_session_dir_exists(home, session_id)),
        Tool::OpenCode => None, // unreachable; kept exhaustive
    }
}

/// Grok stores sessions as `~/.grok/sessions/<encoded-cwd>/<id>/`
/// directories (no per-session jsonl at a fixed depth); the session
/// is resumable iff its dir exists under any cwd group.
fn grok_session_dir_exists(home: &Path, session_id: &str) -> bool {
    let root = home.join(".grok").join("sessions");
    let Ok(groups) = std::fs::read_dir(&root) else {
        return false;
    };
    groups.flatten().any(|g| g.path().join(session_id).is_dir())
}

fn claude_session_jsonl_exists(home: &Path, session_id: &str) -> bool {
    let projects = home.join(".claude").join("projects");
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return false;
    };
    let file_name = format!("{session_id}.jsonl");
    for entry in entries.flatten() {
        if entry.path().join(&file_name).is_file() {
            return true;
        }
    }
    false
}

fn codex_session_jsonl_exists(home: &Path, session_id: &str) -> bool {
    let root = home.join(".codex").join("sessions");
    walk_for_session(&root, session_id, 4)
}

fn walk_for_session(dir: &Path, session_id: &str, depth: usize) -> bool {
    if depth == 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if walk_for_session(&path, session_id, depth - 1) {
                return true;
            }
        } else if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.starts_with("rollout-") && name.ends_with(".jsonl") && name.contains(session_id)
            {
                return true;
            }
        }
    }
    false
}
