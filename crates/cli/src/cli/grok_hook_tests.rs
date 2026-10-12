use super::*;
use crate::agent_env::Stated;
use crate::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::AgentLabel;
use clank_core::agent_config::AgentConfig;
use serde_json::json;

fn fixture() -> (tempfile::TempDir, PathBuf, AgentLabel, SessionId, Stated) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().canonicalize().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    let label = AgentLabel::parse("grok").unwrap();
    crate::cli::agent::add_repo_roster_agent(
        &repo,
        &label,
        AgentDescription {
            tool: Tool::Grok,
            launch: None,
            initial_prompt: None,
        },
        RosterRole::Master,
    )
    .unwrap();
    crate::agent_store::save_agent_config(
        &repo,
        &label,
        &AgentConfig {
            auto_mode: Some(AutoMode::On),
            ..Default::default()
        },
    )
    .unwrap();
    let session = SessionId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let env = Stated::saying(&[
        ("CLANK_AGENT", "grok"),
        ("GROK_SESSION_ID", session.as_str()),
    ])
    .at(&repo, &repo);
    (temp, repo, label, session, env)
}

fn input(repo: &Path, session: &SessionId, event: &str, tasks: serde_json::Value) -> Input {
    serde_json::from_value(json!({"hookEventName":event, "sessionId":session,
        "cwd":repo, "reason":"end_turn", "backgroundTasks":tasks}))
    .unwrap()
}

#[test]
fn stop_uses_native_snapshot_and_never_rewrites_a_command() {
    let (_temp, repo, label, session, env) = fixture();
    handle(
        &input(&repo, &session, "session_start", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    assert!(binding_matches(&repo, &label, Tool::Grok, session.as_str()));
    let mut stop = input(&repo, &session, "stop", json!([]));
    assert_eq!(handle(&stop, &repo, &env).unwrap(), Some(ARM));
    for command in [
        "clank wait",
        "clank wait --repo '/same path'",
        "exec /usr/bin/clank wait",
    ] {
        stop.background_tasks = Some(vec![Task {
            kind: "shell".into(),
            status: "running".into(),
            command: Some(command.into()),
        }]);
        assert_eq!(handle(&stop, &repo, &env).unwrap(), None);
    }
    stop.background_tasks = Some(vec![Task {
        kind: "shell".into(),
        status: "running".into(),
        command: Some("echo clank wait".into()),
    }]);
    assert_eq!(handle(&stop, &repo, &env).unwrap(), Some(ARM));
    stop.background_tasks = Some(vec![]);
    stop.stop_hook_active = true;
    assert_eq!(
        handle(&stop, &repo, &env).unwrap(),
        None,
        "failed or immediate waits must not cause a repair loop"
    );
}

#[test]
fn lifecycle_cannot_release_a_replacement_and_respects_manual_mode() {
    let (_temp, repo, label, session, env) = fixture();
    handle(
        &input(&repo, &session, "session_start", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    crate::agent_store::update_agent_config(&repo, &label, |cfg| {
        cfg.auto_mode = Some(AutoMode::Off)
    })
    .unwrap();
    assert_eq!(
        handle(&input(&repo, &session, "stop", json!([])), &repo, &env).unwrap(),
        None
    );
    let successor = SessionId::parse("22222222-2222-4222-8222-222222222222").unwrap();
    crate::agent_store::bind_session_to_agent(&repo, &label, Tool::Grok, &successor).unwrap();
    handle(
        &input(&repo, &session, "session_end", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    assert!(binding_matches(
        &repo,
        &label,
        Tool::Grok,
        successor.as_str()
    ));
    assert_eq!(
        handle(&input(&repo, &session, "stop", json!([])), &repo, &env).unwrap(),
        None
    );
    handle(
        &input(&repo, &successor, "session_end", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    assert!(!binding_matches(
        &repo,
        &label,
        Tool::Grok,
        successor.as_str()
    ));
}

#[test]
fn automatic_wait_accepts_same_repo_and_rejects_other_repo_or_stale_session() {
    let (_temp, repo, label, session, env) = fixture();
    handle(
        &input(&repo, &session, "session_start", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    let mut args = super::super::WaitArgs {
        repo: None,
        author: None,
        die_with_owner: false,
        deliver_to_session: None,
        json: false,
        events: vec![],
        r#for: None,
        peek: false,
        no_cache: false,
        poll: false,
        no_poll: false,
    };
    assert!(
        super::super::wait::automatic_grok(&args, &env)
            .unwrap()
            .is_some()
    );
    args.repo = Some(repo.join("."));
    assert!(
        super::super::wait::automatic_grok(&args, &env)
            .unwrap()
            .is_some()
    );
    let (_other, other, _, _, _) = fixture();
    args.repo = Some(other);
    assert!(super::super::wait::automatic_grok(&args, &env).is_err());
    args.repo = None;
    let nested = repo.join("docs/deep");
    std::fs::create_dir_all(&nested).unwrap();
    let in_subdir = Stated::saying(&[
        ("CLANK_AGENT", "grok"),
        ("GROK_SESSION_ID", session.as_str()),
    ])
    .at(&repo, &nested);
    let found = super::super::wait::automatic_grok(&args, &in_subdir).unwrap();
    assert_eq!(
        found.map(|(r, ..)| r),
        Some(repo.clone()),
        "from a subdirectory"
    );
    crate::agent_store::release_binding(&repo, &label, &session).unwrap();
    assert!(super::super::wait::automatic_grok(&args, &env).is_err());
}

/// A native Grok session working in a subdirectory of its repo is that
/// repo's: the hook resolves and validates its cwd by the worktree that
/// holds it, an explicit `--repo` stays exact, and a repo that does not
/// hold the session's cwd is still refused.
#[test]
fn a_session_in_a_subdirectory_is_its_repos() {
    let (_temp, repo, label, session, env) = fixture();
    let nested = repo.join("docs/split-backup/formal");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(hook_repo(None, &nested).unwrap(), repo);
    assert_eq!(hook_repo(Some(&nested), &nested).unwrap(), nested);
    handle(
        &input(&nested, &session, "session_start", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    assert!(binding_matches(&repo, &label, Tool::Grok, session.as_str()));
    let stop = input(&nested, &session, "stop", json!([]));
    assert_eq!(handle(&stop, &repo, &env).unwrap(), Some(ARM));

    let (_other, elsewhere, ..) = fixture();
    assert!(
        handle(&stop, &elsewhere, &env).is_err(),
        "not the repo holding its cwd"
    );
}
