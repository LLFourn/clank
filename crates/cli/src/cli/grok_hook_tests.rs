use super::*;
use crate::agent_env::Stated;
use crate::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::AutoMode;
use serde_json::json;

const ARM: &str = crate::cli::wake_render::ARM_WAIT;

/// A native event as Grok is answered: the evidence routed here, decided
/// by the monitor's own rule, framed on Grok's wire. The reason the Stop
/// is blocked with, if it is.
fn handle(input: &Input, repo: &Path, env: &dyn Ambient) -> Result<Option<String>> {
    let Some((label, handoff)) = route(input, repo, env)? else {
        return Ok(None);
    };
    let g = crate::cli::turn_custody::Gathered::of(repo, &label, None, Err(String::new()));
    let frame = crate::cli::turn_custody::frame_of(g.handoff_verdict(&handoff));
    Ok(wire(&frame).0.map(|out| {
        let block: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(block["decision"], "block");
        block["reason"].as_str().unwrap().to_string()
    }))
}

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
    crate::agent_store::set_auto_mode(&repo, &label, AutoMode::On).unwrap();
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
    assert_eq!(handle(&stop, &repo, &env).unwrap(), Some(ARM.to_string()));
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
    assert_eq!(handle(&stop, &repo, &env).unwrap(), Some(ARM.to_string()));
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
        turn_end: false,
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
    assert_eq!(handle(&stop, &repo, &env).unwrap(), Some(ARM.to_string()));

    let (_other, elsewhere, ..) = fixture();
    assert!(
        handle(&stop, &elsewhere, &env).is_err(),
        "not the repo holding its cwd"
    );
}

/// The monitor's answer on Grok's wire: a request for the wait blocks the
/// Stop with the monitor's own reason, a diagnostic goes to stderr naming
/// the manual way out, and a silence says nothing.
#[test]
fn the_wire_frames_what_the_monitor_said() {
    let (out, err) = wire(&Frame::text(WireVerdict::Notify, ARM.to_string()));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out.unwrap()).unwrap(),
        json!({"decision": "block", "reason": ARM})
    );
    assert_eq!(err, None);
    let (out, err) = wire(&Frame::bare(WireVerdict::Diagnostic {
        message: "no role".into(),
    }));
    assert_eq!(out, None);
    assert!(err.unwrap().contains("no role"));
    assert_eq!(
        wire(&Frame::bare(WireVerdict::Silent {
            why: clank_core::SilentReason::WaitArmed
        })),
        (None, None)
    );
}

/// A request for the wait that was in transit while the label was rebound
/// reaches the old session as nothing: the binding is checked again once
/// the monitor's answer, and its child's collection, are done.
#[tokio::test]
async fn an_answer_overtaken_by_a_rebind_reaches_the_old_session_as_nothing() {
    let (_temp, repo, label, session, env) = fixture();
    handle(
        &input(&repo, &session, "session_start", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let asked = async {
        held.await.unwrap();
        Frame::text(WireVerdict::Notify, ARM.to_string())
    };
    let successor = SessionId::parse("22222222-2222-4222-8222-222222222222").unwrap();
    let rebind_then_release = async {
        crate::agent_store::bind_session_to_agent(&repo, &label, Tool::Grok, &successor).unwrap();
        release.send(()).unwrap();
    };
    let (frame, ()) = tokio::join!(
        still_bound(&repo, &label, session.as_str(), asked),
        rebind_then_release
    );
    assert_eq!(wire(&frame), (None, None), "no block, no arm: {frame:?}");

    let answered = still_bound(&repo, &label, successor.as_str(), async {
        Frame::text(WireVerdict::Notify, ARM.to_string())
    })
    .await;
    assert!(
        wire(&answered).0.is_some(),
        "the session holding the label is answered"
    );
}
