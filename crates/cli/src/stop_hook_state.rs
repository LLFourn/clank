//! Per-(label, session) state file for the Stop-hook progress guard.
//!
//! Tracks a hash of the agent's last assistant message across hook
//! fires so we can distinguish "agent did real work since last fire"
//! from "agent re-emitted the same message" (a real spin).
//!
//! Reads return `Ok(None)` only for legitimate NotFound; other IO
//! errors propagate as `Err` so callers can fail closed.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use clank_core::ids::{AgentLabel, SessionId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopHookState {
    pub last_assistant_message_hash: String,
    pub fired_at: String,
}

pub fn state_path(repo: &Path, label: &AgentLabel, session: &SessionId) -> PathBuf {
    repo.join(".clank")
        .join("agents")
        .join(label.as_str())
        .join("stop-hook-state")
        .join(format!("{}.json", session.as_str()))
}

pub fn read_state(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
) -> std::io::Result<Option<StopHookState>> {
    let path = state_path(repo, label, session);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<StopHookState>(&raw) {
            Ok(state) => Ok(Some(state)),
            Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("parse `{}`: {e}", path.display()),
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn write_state(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
    state: &StopHookState,
) -> std::io::Result<()> {
    let path = state_path(repo, label, session);
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("no parent for `{}`", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-stophook-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    let json = serde_json::to_string(state).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("serialize: {e}"))
    })?;
    tmp.write_all(json.as_bytes())?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(())
}

pub fn hash_progress(msg: &str) -> String {
    blake3::hash(msg.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label() -> AgentLabel {
        AgentLabel::parse("alice").unwrap()
    }
    fn session() -> SessionId {
        SessionId::parse("abcdef0123456789").unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = StopHookState {
            last_assistant_message_hash: hash_progress("hello"),
            fired_at: "2026-05-23T00:00:00Z".into(),
        };
        write_state(dir.path(), &label(), &session(), &state).unwrap();
        let loaded = read_state(dir.path(), &label(), &session())
            .unwrap()
            .unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn read_missing_returns_ok_none() {
        let dir = tempfile::tempdir().unwrap();
        let got = read_state(dir.path(), &label(), &session()).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn read_corrupt_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path(), &label(), &session());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        let err = read_state(dir.path(), &label(), &session()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn hash_is_deterministic_and_distinguishes() {
        assert_eq!(hash_progress("a"), hash_progress("a"));
        assert_ne!(hash_progress("a"), hash_progress("b"));
    }
}
