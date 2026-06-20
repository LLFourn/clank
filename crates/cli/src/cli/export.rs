//! `clank export` — dump this repo's self-contained config
//! (`teams_config::RepoConfigFile`: the `agents` roster) as pretty
//! JSON to stdout.
//!
//! Read through the same fail-closed loader the resolver uses, so
//! an old-shape config produces the re-init hint instead of a
//! garbled dump. Plain serialization: no resolution, no
//! validation beyond what the loader enforces.

use std::path::Path;

use crate::cli::{ExportArgs, resolve_repo};

pub async fn run(args: ExportArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    export_repo_config(&repo)
}

/// Serialize `<repo>/.clank/config.json` (the new-shape
/// [`RepoConfigFile`]) as pretty JSON to stdout. `pub`, env-free
/// (explicit `repo`) so integration tests can round-trip the
/// printed JSON. Fail-closed via
/// [`crate::agent_store::load_repo_config_required`].
pub fn export_repo_config(repo: &Path) -> anyhow::Result<()> {
    let cfg = crate::agent_store::load_repo_config_required(repo)?;
    println!("{}", serde_json::to_string_pretty(&cfg)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::teams_config::RepoConfigFile;

    fn seed_repo_config(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join(".clank")).unwrap();
        std::fs::write(repo.join(".clank/config.json"), body).unwrap();
    }

    /// `export_repo_config` prints JSON that round-trips back into
    /// a `RepoConfigFile` with the same roster. We capture stdout by
    /// re-serializing the loaded config (the function prints exactly
    /// that), then parse it back.
    #[test]
    fn export_round_trips_roster() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::ids::AgentLabel;
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": {
                    "claude": { "tool": "claude", "role": "master" },
                    "codex": { "tool": "codex", "role": "commit" }
                }
            }"#,
        );

        // Load through the same path export uses, then assert the
        // printed JSON parses back equal.
        let cfg = crate::agent_store::load_repo_config_required(repo.path()).unwrap();
        let printed = serde_json::to_string_pretty(&cfg).unwrap();
        let back: RepoConfigFile = serde_json::from_str(&printed).unwrap();
        assert_eq!(back.agents.len(), 2);
        assert_eq!(
            back.agents[&AgentLabel::parse("claude").unwrap()].role,
            RosterRole::Master
        );
        assert_eq!(back.agents, cfg.agents);
        // And the public entry point succeeds.
        export_repo_config(repo.path()).unwrap();
    }

    #[test]
    fn export_fails_closed_on_old_shape() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(repo.path(), r#"{"team": "dev", "promoted": "codex"}"#);
        let err = export_repo_config(repo.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn export_errors_when_no_repo_config() {
        let repo = tempfile::tempdir().unwrap();
        let err = export_repo_config(repo.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no repo config") && msg.contains("clank init"),
            "expected init hint; got: {msg}"
        );
    }
}
