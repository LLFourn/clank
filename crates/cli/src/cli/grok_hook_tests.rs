use super::*;
use crate::agent_env::Stated;
use crate::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::AutoMode;
use serde_json::json;

const ARM: &str = crate::cli::wake_render::ARM_WAIT;

/// A native event as Grok is answered while the monitor does (`true`) or
/// does not hold a registration of the session's wait: routed here,
/// decided by the monitor's own rule, framed on Grok's wire. The reason
/// the Stop is blocked with, if it is.
fn handle_with(
    input: &Input,
    repo: &Path,
    env: &dyn Ambient,
    wait_registered: bool,
) -> Result<Option<String>> {
    let Some(label) = route(input, repo, env)? else {
        return Ok(None);
    };
    let g = crate::cli::turn_custody::Gathered::of(repo, &label, None).waiting(wait_registered);
    let frame = crate::cli::turn_custody::frame_of(
        g.handoff_verdict(&input.session_id, input.stop_hook_active),
    );
    Ok(wire(&frame).0.map(|out| {
        let block: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(block["decision"], "block");
        block["reason"].as_str().unwrap().to_string()
    }))
}

fn handle(input: &Input, repo: &Path, env: &dyn Ambient) -> Result<Option<String>> {
    handle_with(input, repo, env, false)
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

/// The waiter is never read from the task list: a listed running `clank
/// wait` and an absent `backgroundTasks` decide the same. Only the monitor's
/// registration of the wait says it runs, and the continuation of the turn
/// asked to arm one treats its not-yet-registered wait as pending.
#[test]
fn the_waiter_is_the_monitors_registration_never_a_task_list() {
    let (_temp, repo, label, session, env) = fixture();
    handle(
        &input(&repo, &session, "session_start", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    assert!(binding_matches(&repo, &label, Tool::Grok, session.as_str()));
    let listed = input(
        &repo,
        &session,
        "stop",
        json!([{"type": "shell", "status": "running", "command": "clank wait"}]),
    );
    let bare: Input = serde_json::from_value(json!({"hookEventName": "stop",
        "sessionId": session, "cwd": repo, "reason": "end_turn"}))
    .unwrap();
    for stop in [&listed, &bare] {
        assert_eq!(handle(stop, &repo, &env).unwrap(), Some(ARM.to_string()));
        assert_eq!(handle_with(stop, &repo, &env, true).unwrap(), None);
    }
    let mut continued = bare;
    continued.stop_hook_active = true;
    assert_eq!(
        handle(&continued, &repo, &env).unwrap(),
        None,
        "a wait just armed may not have registered yet; a failed one must not loop"
    );
}

/// A native Stop reports Grok's `backgroundTasks` as sent, for the monitor
/// to resolve and end declarations by: an omitted field is no listing, and
/// an empty one says nothing runs.
#[test]
fn a_stop_reports_its_native_listing_and_an_omitted_one_as_none() {
    let (_temp, repo, _, session, _) = fixture();
    let subagent = json!({"id": "g1", "type": "subagent", "status": "running",
        "description": "sleeper", "agentType": "explore"});
    let tasks = |stop: Input| match report(stop, 0) {
        Report::Handoff { tasks, .. } => tasks,
        other => panic!("{other:?}"),
    };
    let listed = tasks(input(&repo, &session, "stop", json!([subagent.clone()])));
    let TaskSnapshot::Known(listed) = listed else {
        panic!("{listed:?}")
    };
    let [task] = listed.as_slice() else {
        panic!("{listed:?}")
    };
    assert_eq!(task.id.as_deref(), Some("g1"));
    assert_eq!(task.description.as_deref(), Some("sleeper"));
    assert_eq!(task.raw, subagent, "kept as sent");
    assert_eq!(
        tasks(input(&repo, &session, "stop", json!([]))),
        TaskSnapshot::Known(vec![])
    );
    let bare: Input = serde_json::from_value(json!({"hookEventName": "stop",
        "sessionId": session, "cwd": repo, "reason": "end_turn"}))
    .unwrap();
    assert_eq!(tasks(bare), TaskSnapshot::Unavailable);
}

/// A Grok start binds its session only while it is unbound: a session
/// already bound keeps its label whatever `CLANK_AGENT` it inherited.
#[test]
fn a_start_never_moves_a_bound_session() {
    let (_temp, repo, label, session, _) = fixture();
    let other = AgentLabel::parse("other").unwrap();
    crate::agent_store::bind_session_to_agent(&repo, &other, Tool::Grok, &session).unwrap();
    let env = Stated::saying(&[("CLANK_AGENT", label.as_str())]).at(&repo, &repo);
    handle(
        &input(&repo, &session, "session_start", json!([])),
        &repo,
        &env,
    )
    .unwrap();
    assert!(binding_matches(&repo, &other, Tool::Grok, session.as_str()));
    assert!(!binding_matches(
        &repo,
        &label,
        Tool::Grok,
        session.as_str()
    ));
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

/// The notices a child with no monitor prints for Grok's Stop block it
/// with their reason, which is how Grok is woken: the child's own frames,
/// with nothing of the work the repo owes.
#[tokio::test(start_paused = true)]
async fn the_childs_no_monitor_notices_block_groks_stop_with_the_reason() {
    use crate::cli::turn_child::tests::{handoff, newer, no_binary, unserved};
    for (attempt, says) in [
        (
            no_binary as fn() -> crate::cli::monitor::Attempt,
            "clank cannot start its monitor: starting one failed: no binary.",
        ),
        (
            newer,
            "clank cannot start its monitor: the monitor speaks a newer protocol.",
        ),
    ] {
        let [frame] = unserved(&handoff(), attempt)
            .await
            .try_into()
            .expect("one frame");
        assert!(frame.presentation().is_none(), "{frame:?}");
        let (out, err) = wire(&frame);
        assert_eq!(err, None);
        let block: serde_json::Value = serde_json::from_str(&out.expect("a decision")).unwrap();
        assert_eq!(block["decision"], "block");
        let reason = block["reason"].as_str().unwrap();
        assert!(reason.starts_with(says), "{reason}");
        assert!(!reason.contains("scheduled work"), "{reason}");
    }
}

/// The child's ordinary diagnostic still blocks nothing on Grok's wire.
#[tokio::test]
async fn the_childs_diagnostic_still_blocks_nothing() {
    let frame = crate::cli::turn_child::tests::unparsed_frame().await;
    let (out, err) = wire(&frame);
    assert_eq!(out, None);
    assert!(err.is_some_and(|e| e.contains("did not parse")));
}

/// A handoff with no monitor is told so before its hook stops waiting:
/// the hook gives up at the instant its report names, and a deadline it
/// framed itself would block nothing.
#[tokio::test(start_paused = true)]
async fn a_handoff_with_no_monitor_is_told_so_before_its_hook_gives_up() {
    use crate::cli::turn_child::tests::{handoff_by, no_binary, unserved, within};
    let began = tokio::time::Instant::now();
    let [frame] = unserved(&handoff_by(within(HANDOFF_BUDGET)), no_binary)
        .await
        .try_into()
        .expect("one frame");
    let spare = HANDOFF_BUDGET.saturating_sub(began.elapsed());
    assert!(
        spare >= std::time::Duration::from_secs(1),
        "the hook had {spare:?} of its wait left to read it in"
    );
    let (out, err) = wire(&frame);
    assert_eq!(err, None);
    let block: serde_json::Value = serde_json::from_str(&out.expect("a decision")).unwrap();
    assert_eq!(block["decision"], "block");
    assert!(
        block["reason"]
            .as_str()
            .unwrap()
            .starts_with("clank cannot start its monitor:")
    );
}
