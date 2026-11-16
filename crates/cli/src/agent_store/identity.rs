//! Binding-only access for provider adapters. Policy fields stay opaque even
//! when they share a JSON document with session identity.
use super::*;

#[derive(Default, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub(super) struct Identity {
    pub session: Option<Session>,
    pub resume_pending: Option<ResumePending>,
}

pub(crate) fn binding(repo: &Path, label: &AgentLabel) -> anyhow::Result<Option<Session>> {
    super::binding_in(&agents_root(repo).join(label.as_str()))
}

pub(crate) fn bindings(repo: &Path, lossy: bool) -> anyhow::Result<Vec<(AgentLabel, Session)>> {
    let mut out = Vec::new();
    for entry in iter_agent_dirs(repo)? {
        let read = entry.and_then(|(label, dir)| Ok((label, super::binding_in(&dir)?)));
        match read {
            Ok((label, Some(session))) => out.push((label, session)),
            Ok((_, None)) => {}
            Err(_) if lossy => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Bootstrap needs only the roster's declared tool, never role or launch policy.
pub(crate) fn tool(repo: &Path, label: &AgentLabel) -> anyhow::Result<Tool> {
    let path = repo_config::path(repo);
    let doc = versioned::read(&path, repo_config::VERSION)?.context("no roster configured")?;
    anyhow::ensure!(
        !doc.contains_key("team") && !doc.contains_key("promoted"),
        "legacy roster configuration"
    );
    let entry = doc
        .get("agents")
        .and_then(|a| a.as_object())
        .and_then(|a| a.get(label.as_str()))
        .context("agent is not in the roster")?;
    anyhow::ensure!(entry.get("role").is_some(), "legacy roster entry");
    serde_json::from_value(
        entry
            .get(crate::cli::teams_config::ROSTER_TOOL_FIELD)
            .context("roster entry has no tool")?
            .clone(),
    )
    .map_err(Into::into)
}

/// Patch only changed identity fields under the same lock used by policy
/// writers. Unknown and malformed policy values are preserved, not parsed.
pub(super) fn mutate<T>(
    guard: &BindingGuard,
    edit: impl FnOnce(&mut Identity) -> T,
) -> anyhow::Result<T> {
    let path = agent_config_path(guard.repo, guard.label);
    let mut doc = versioned::read(&path, AGENT_CONFIG_VERSION)?.unwrap_or_default();
    let before: Identity = serde_json::from_value(serde_json::Value::Object(doc.clone()))?;
    let mut after = before.clone();
    let result = edit(&mut after);
    #[cfg(test)]
    tests::inside_edit();
    if after != before {
        let before = serde_json::to_value(before)?;
        let after = serde_json::to_value(after)?;
        for key in ["session", "resume_pending"] {
            if before[key] != after[key] {
                if after[key].is_null() {
                    doc.remove(key);
                } else {
                    doc.insert(key.into(), after[key].clone());
                }
            }
        }
        let _permit = crate::upgrade::writing()?;
        versioned::write(&path, &doc, AGENT_CONFIG_VERSION)?;
    }
    Ok(result)
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn hook_identity_ignores_policy_and_preserves_it_during_binding_changes() {
        let repo = tempfile::tempdir().unwrap();
        let label = AgentLabel::parse("agent").unwrap();
        let session = SessionId::parse("native-session").unwrap();
        let path = agent_config_path(repo.path(), &label);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let policy = serde_json::json!({"auto_mode": ["invalid"], "wait_events": "invalid", "outage_notified": 123});
        std::fs::write(&path, policy.to_string()).unwrap();
        let roster = serde_json::json!({"agents": {"agent": {"tool":"claude", "role":"master", "launch":42}}, "event_watches": "invalid", "review": false});
        std::fs::write(repo_config::path(repo.path()), roster.to_string()).unwrap();
        assert!(load_agent_config(repo.path(), &label).is_err());
        assert_eq!(tool(repo.path(), &label).unwrap(), Tool::Claude);
        assert!(matches!(
            crate::agent_env::session_start(repo.path(), Tool::Claude, &session, Some(&label))
                .unwrap(),
            crate::agent_env::Start::Bootstrap(_)
        ));
        bind_session_to_agent(repo.path(), &label, Tool::Claude, &session).unwrap();
        assert_eq!(
            crate::agent_env::resolve_identity_for_hook_in(repo.path(), Tool::Claude, &session)
                .unwrap(),
            label
        );
        assert!(binding_matches(
            repo.path(),
            &label,
            Tool::Claude,
            session.as_str()
        ));
        record_transcript(repo.path(), &label, &session, "/tmp/transcript").unwrap();
        assert_eq!(
            binding(repo.path(), &label)
                .unwrap()
                .unwrap()
                .transcript
                .as_deref(),
            Some("/tmp/transcript")
        );
        assert!(release_binding(repo.path(), &label, &session).unwrap());
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        for key in ["auto_mode", "wait_events", "outage_notified"] {
            assert_eq!(after[key], policy[key]);
        }
    }

    /// The rosters the two readers are compared over: the format fixture,
    /// and one entry for each tool in each role, with and without launch
    /// policy.
    fn rosters() -> Vec<String> {
        use crate::cli::teams_config::RosterRole;
        let tools = [Tool::Claude, Tool::Codex, Tool::Grok, Tool::OpenCode];
        let roles = [
            RosterRole::Master,
            RosterRole::Commit,
            RosterRole::Plan,
            RosterRole::Final,
            RosterRole::Gate,
            RosterRole::Standby,
        ];
        let mut agents = serde_json::Map::new();
        for (i, tool) in tools.iter().enumerate() {
            for (j, role) in roles.iter().enumerate() {
                let mut entry = serde_json::json!({ "tool": tool, "role": role });
                if j % 2 == 1 {
                    entry["launch"] = serde_json::json!({ "args": ["--profile", "x"] });
                    entry["initial_prompt"] = "go".into();
                }
                agents.insert(format!("agent{i}x{j}"), entry);
            }
        }
        vec![
            include_str!("../../tests/fixtures/formats/repo-config.json").to_string(),
            serde_json::json!({ "agents": agents }).to_string(),
        ]
    }

    #[test]
    fn the_binding_reader_and_the_roster_parser_agree_on_every_tool() {
        use crate::cli::teams_config::{ROSTER_TOOL_FIELD, RosterAgent, RosterRole};
        let entry = RosterAgent {
            tool: Tool::Grok,
            launch: None,
            initial_prompt: None,
            role: RosterRole::Master,
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap()[ROSTER_TOOL_FIELD],
            serde_json::json!(Tool::Grok),
            "the roster parser keeps the tool in the shared field"
        );
        for roster in rosters() {
            let repo = tempfile::tempdir().unwrap();
            let path = repo_config::path(repo.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &roster).unwrap();
            let parsed = repo_config::read(repo.path())
                .expect("the roster parser reads the fixture")
                .expect("the fixture is a roster");
            assert!(!parsed.agents.is_empty());
            for (label, agent) in &parsed.agents {
                assert_eq!(
                    tool(repo.path(), label).unwrap(),
                    agent.tool,
                    "{label} in {roster}"
                );
            }
            let absent = AgentLabel::parse("nobody").unwrap();
            assert!(tool(repo.path(), &absent).is_err());
        }
        let repo = tempfile::tempdir().unwrap();
        let path = repo_config::path(repo.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            include_str!("../../tests/fixtures/formats/repo-config-legacy.json"),
        )
        .unwrap();
        assert!(repo_config::read(repo.path()).is_err());
        for label in ["claude", "codex"] {
            let label = AgentLabel::parse(label).unwrap();
            assert!(
                tool(repo.path(), &label).is_err(),
                "neither reader takes a tool from a legacy roster"
            );
        }
    }

    #[test]
    fn invalid_or_missing_binding_never_authorizes_a_hook() {
        let repo = tempfile::tempdir().unwrap();
        let label = AgentLabel::parse("agent").unwrap();
        assert!(!binding_matches(
            repo.path(),
            &label,
            Tool::Claude,
            "native-session"
        ));
        let path = agent_config_path(repo.path(), &label);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for bytes in ["not json", r#"{"session":false}"#] {
            std::fs::write(&path, bytes).unwrap();
            assert!(!binding_matches(
                repo.path(),
                &label,
                Tool::Claude,
                "native-session"
            ));
            assert!(
                bind_session_to_agent(
                    repo.path(),
                    &label,
                    Tool::Claude,
                    &SessionId::parse("native-session").unwrap()
                )
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        }
    }
}
