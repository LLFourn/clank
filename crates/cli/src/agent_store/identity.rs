//! Binding-only access for provider adapters. Every other part of an
//! agent's state stays opaque, even though it shares the document.
use super::*;

#[derive(Default, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub(super) struct Identity {
    pub binding: Option<Session>,
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

/// Patch only the identity fields, under the agent's lock. Binding a
/// different session than the one bound (or none) drops everything known
/// of the old one, `bound` and its unconfirmed resume, in the same write.
/// The rest of the document is preserved unparsed; one an older build
/// wrote, with no version, holds nothing to preserve.
pub(super) fn mutate<T>(
    guard: &StateGuard,
    edit: impl FnOnce(&mut Identity) -> T,
) -> anyhow::Result<T> {
    let path = state::path(guard.agent_dir());
    let mut doc = match versioned::read_stored(&path, state::VERSION)? {
        Some(stored) if stored.versioned => stored.doc,
        _ => Default::default(),
    };
    let before: Identity = serde_json::from_value(serde_json::Value::Object(doc.clone()))
        .map_err(|e| Refusal::malformed(&path, e))?;
    let mut after = before.clone();
    let result = edit(&mut after);
    #[cfg(test)]
    tests::inside_edit();
    if after != before {
        let id = |i: &Identity| i.binding.as_ref().map(|s| s.id.clone());
        if id(&after) != id(&before) {
            after.resume_pending = None;
            doc.remove("bound");
        }
        let after = serde_json::to_value(after)?;
        for key in ["binding", "resume_pending"] {
            if after[key].is_null() {
                doc.remove(key);
            } else {
                doc.insert(key.into(), after[key].clone());
            }
        }
        let _permit = crate::upgrade::writing()?;
        versioned::write(&path, &doc, state::VERSION)?;
    }
    Ok(result)
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn hook_identity_ignores_every_other_part_and_preserves_it_during_binding_changes() {
        let repo = tempfile::tempdir().unwrap();
        let label = AgentLabel::parse("agent").unwrap();
        let session = SessionId::parse("native-session").unwrap();
        let dir = agents_root(repo.path()).join(label.as_str());
        std::fs::create_dir_all(&dir).unwrap();
        let policy = serde_json::json!({"auto_mode": ["invalid"], "wait_events": "invalid"});
        std::fs::write(config_in(&dir), policy.to_string()).unwrap();
        let kept = serde_json::json!({"version": 1, "issued": "invalid", "ledger": 123});
        std::fs::write(state::path(&dir), kept.to_string()).unwrap();
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
            serde_json::from_slice(&std::fs::read(state::path(&dir)).unwrap()).unwrap();
        for key in ["issued", "ledger"] {
            assert_eq!(after[key], kept[key]);
        }
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config_in(&dir)).unwrap()).unwrap();
        assert_eq!(config, policy, "a binding never touches the config");
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
        let path = state::path(&agents_root(repo.path()).join(label.as_str()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for bytes in ["not json", r#"{"version":1,"binding":false}"#] {
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
