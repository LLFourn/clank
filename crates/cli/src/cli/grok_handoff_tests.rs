use super::*;
use crate::agent_env::Stated;
use crate::cli::teams_config::{RepoConfigFile, RosterAgent, RosterRole};
use clank_core::agent_config::AgentConfig;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

const SESSION: &str = "11111111-1111-4111-8111-111111111111";

struct Fixture {
    _temp: tempfile::TempDir,
    runtime: Runtime,
    owner: Owner,
    env: Stated,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let repo = temp.path().canonicalize().unwrap();
        let label = AgentLabel::parse("grok").unwrap();
        std::fs::create_dir(repo.join(".clank")).unwrap();
        let mut roster = RepoConfigFile::default();
        roster.agents.insert(
            label.clone(),
            RosterAgent {
                tool: Tool::Grok,
                launch: None,
                initial_prompt: None,
                role: RosterRole::Master,
            },
        );
        std::fs::write(
            repo.join(".clank/config.json"),
            serde_json::to_vec(&roster).unwrap(),
        )
        .unwrap();
        let session = SessionId::parse(SESSION).unwrap();
        crate::agent_store::bind_session_to_agent(&repo, &label, Tool::Grok, &session).unwrap();
        crate::agent_store::set_auto_mode(&repo, &label, AutoMode::On).unwrap();
        let env = Stated::saying(&[
            (crate::agent_env::ENV_CLANK_AGENT, "grok"),
            (crate::agent_env::ENV_GROK_SESSION, SESSION),
        ])
        .at(&repo, &repo);
        Self {
            _temp: temp,
            runtime: Runtime::production().unwrap(),
            owner: Owner {
                repo,
                label,
                session,
            },
            env,
        }
    }
    fn input(&self, event: &str) -> Input {
        serde_json::from_value(serde_json::json!({
            "hookEventName":event, "sessionId":self.owner.session,
            "cwd":self.owner.repo, "promptId":"turn-1", "reason":"end_turn",
            "toolName":"run_terminal_command",
            "toolUseId":"call-1",
            "toolInput":{"command":"clank grok-wait", "block_until_ms":0, "description":"Clank automatic wait"}
        })).unwrap()
    }
    fn offer(&self) -> Input {
        let mut input = self.input("pre_tool_use");
        let output = handle(&self.runtime, &input, &self.owner.repo, &self.env).unwrap();
        input.tool_input = output.specific.unwrap().updated_input.unwrap();
        input
    }
    fn registration(&self, offer: &Input) -> Input {
        let mut post = self.input("post_tool_use");
        post.tool_input = offer.tool_input.clone();
        post.tool_result = serde_json::json!({"type":"BackgroundTaskStarted", "task_id":"task-1",
            "task_type":"bash", "status":"running", "command":offer.tool_input["command"]});
        post
    }
    fn confirm(&self, offer: &Input) {
        let post = self.registration(offer);
        let output = handle(&self.runtime, &post, &self.owner.repo, &self.env).unwrap();
        assert!(output.specific.is_none());
    }
    fn worker(&self, offer: &Input) -> Worker<'_> {
        let (key, generation) = self
            .runtime
            .command_args(offer.tool_input["command"].as_str().unwrap())
            .unwrap();
        let worker = Worker::start(&self.runtime, key, generation, &self.env).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let socket = self.runtime.path(&self.owner.key(), "sock").unwrap();
        assert_eq!(
            std::fs::metadata(socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        worker
    }
    fn record(&self) -> Record {
        self.runtime.read(&self.owner.key()).unwrap().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for extension in ["json", "sock", "lock", "lease"] {
            let _ = std::fs::remove_file(self.runtime.path(&self.owner.key(), extension).unwrap());
        }
    }
}

#[tokio::test]
async fn immediately_pending_work_is_not_polled_until_provider_registration() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    let worker = fixture.worker(&offer);
    let polls = AtomicUsize::new(0);
    let work = async {
        polls.fetch_add(1, Ordering::SeqCst);
        Ok("pending event")
    };
    let delivery = worker.run(&fixture.env, work);
    tokio::pin!(delivery);
    assert!(matches!(futures_util::poll!(&mut delivery), Poll::Pending));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    fixture.confirm(&offer);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut delivery)
            .await
            .unwrap()
            .unwrap(),
        "pending event"
    );
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert!(matches!(fixture.record().phase, Phase::Delivered { .. }));
    // Completion may beat Stop, but is known to be provider-backgrounded.
    assert!(
        stop(
            &fixture.runtime,
            &fixture.owner,
            &fixture.input("stop"),
            &fixture.env
        )
        .unwrap()
        .decision
        .is_none()
    );
    let mut next = fixture.input("stop");
    next.prompt_id = Some("turn-2".into());
    assert_eq!(
        stop(&fixture.runtime, &fixture.owner, &next, &fixture.env)
            .unwrap()
            .decision,
        Some("block")
    );
}

#[tokio::test]
async fn registration_before_worker_start_is_latched_and_stop_accepts_starting_task() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    fixture.confirm(&offer);
    let mut input = fixture.input("stop");
    input.background_tasks = Some(vec![Task {
        id: "task-1".into(),
        kind: "shell".into(),
        status: "running".into(),
        command: Some(offer.tool_input["command"].as_str().unwrap().into()),
    }]);
    assert!(
        stop(&fixture.runtime, &fixture.owner, &input, &fixture.env)
            .unwrap()
            .decision
            .is_none()
    );
    let worker = fixture.worker(&offer);
    assert_eq!(
        worker.run(&fixture.env, async { Ok(42) }).await.unwrap(),
        42
    );
}

#[tokio::test]
async fn idle_wait_receives_work_and_registered_live_worker_satisfies_stop() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    let worker = fixture.worker(&offer);
    fixture.confirm(&offer);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let delivery = worker.run(&fixture.env, async { Ok(rx.await?) });
    tokio::pin!(delivery);
    assert!(matches!(futures_util::poll!(&mut delivery), Poll::Pending));
    assert!(
        stop(
            &fixture.runtime,
            &fixture.owner,
            &fixture.input("stop"),
            &fixture.env
        )
        .unwrap()
        .decision
        .is_none()
    );
    tx.send("next commit").unwrap();
    assert_eq!(delivery.await.unwrap(), "next commit");
}

#[tokio::test]
async fn duplicate_offers_and_workers_do_not_replace_the_original_generation() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    let generation = fixture.record().generation;
    assert_eq!(
        pre(
            &fixture.runtime,
            &fixture.owner,
            &fixture.input("pre_tool_use"),
            &fixture.env
        )
        .unwrap()
        .decision,
        Some("deny")
    );
    let _worker = fixture.worker(&offer);
    assert!(
        Worker::start(
            &fixture.runtime,
            fixture.owner.key(),
            generation.clone(),
            &fixture.env
        )
        .is_err()
    );
    assert_eq!(fixture.record().generation, generation);
    let mut replay = fixture.input("pre_tool_use");
    replay.tool_input = offer.tool_input;
    replay.tool_use_id = Some("call-2".into());
    assert_eq!(
        pre(&fixture.runtime, &fixture.owner, &replay, &fixture.env)
            .unwrap()
            .decision,
        Some("deny")
    );
}

#[tokio::test]
async fn auto_off_config_mutation_wakes_and_cancels_only_the_automatic_worker() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    let worker = fixture.worker(&offer);
    fixture.confirm(&offer);
    let delivery = worker.run(&fixture.env, std::future::pending::<Result<()>>());
    tokio::pin!(delivery);
    assert!(matches!(futures_util::poll!(&mut delivery), Poll::Pending));
    crate::agent_store::set_auto_mode(&fixture.owner.repo, &fixture.owner.label, AutoMode::Off)
        .unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), &mut delivery)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("auto mode is off"));
    assert!(matches!(fixture.record().phase, Phase::Cancelled));
    let mut manual = fixture.input("pre_tool_use");
    manual.tool_input["command"] = "clank wait".into();
    assert!(
        pre(&fixture.runtime, &fixture.owner, &manual, &fixture.env)
            .unwrap()
            .decision
            .is_none()
    );
}

#[tokio::test]
async fn replacement_fences_delivery_and_late_registration_cannot_take_back_the_slot() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    let worker = fixture.worker(&offer);
    fixture.confirm(&offer);
    let delivery = worker.run(&fixture.env, std::future::pending::<Result<()>>());
    tokio::pin!(delivery);
    assert!(matches!(futures_util::poll!(&mut delivery), Poll::Pending));
    let next = SessionId::parse("22222222-2222-4222-8222-222222222222").unwrap();
    crate::agent_store::bind_session_to_agent(
        &fixture.owner.repo,
        &fixture.owner.label,
        Tool::Grok,
        &next,
    )
    .unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), &mut delivery)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("no longer owns"));
    assert!(matches!(fixture.record().phase, Phase::Cancelled));
    let post = fixture.registration(&offer);
    handle(&fixture.runtime, &post, &fixture.owner.repo, &fixture.env).unwrap();
    assert!(matches!(fixture.record().phase, Phase::Cancelled));
    assert_eq!(
        load_agent_config(&fixture.owner.repo, &fixture.owner.label)
            .unwrap()
            .unwrap()
            .session
            .unwrap()
            .id,
        next
    );
}

#[tokio::test]
async fn resume_cancels_previous_generation_and_binds_actual_provider_not_roster_tool() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    let worker = fixture.worker(&offer);
    let delivery = worker.run(&fixture.env, std::future::pending::<Result<()>>());
    tokio::pin!(delivery);
    assert!(matches!(futures_util::poll!(&mut delivery), Poll::Pending));
    let path = fixture.owner.repo.join(".clank/config.json");
    let mut roster: RepoConfigFile =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    roster.agents.get_mut(&fixture.owner.label).unwrap().tool = Tool::Claude;
    std::fs::write(path, serde_json::to_vec(&roster).unwrap()).unwrap();
    let output = handle(
        &fixture.runtime,
        &fixture.input("session_start"),
        &fixture.owner.repo,
        &fixture.env,
    )
    .unwrap();
    assert!(output.specific.is_none(), "SessionStart stdout is passive");
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut delivery)
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert_eq!(
        load_agent_config(&fixture.owner.repo, &fixture.owner.label)
            .unwrap()
            .unwrap()
            .session
            .unwrap()
            .tool,
        Tool::Grok
    );
}

#[tokio::test]
async fn failed_or_unsupported_registration_never_polls_work() {
    for invalid in [
        "foreground",
        "wrong_task_type",
        "wrong_command",
        "missing_id",
        "truncated",
        "wrong_turn",
        "failure",
        "permission",
    ] {
        let fixture = Fixture::new();
        let offer = fixture.offer();
        let worker = fixture.worker(&offer);
        let mut post = fixture.registration(&offer);
        match invalid {
            "foreground" => post.tool_result["type"] = "Bash".into(),
            "wrong_task_type" => post.tool_result["task_type"] = "monitor".into(),
            "wrong_command" => post.tool_result["command"] = "unrelated".into(),
            "missing_id" => post.tool_result["task_id"] = "".into(),
            "truncated" => post.tool_result_truncated = true,
            "wrong_turn" => post.prompt_id = Some("another-turn".into()),
            "failure" => post.hook_event_name = "post_tool_use_failure".into(),
            "permission" => post.hook_event_name = "permission_denied".into(),
            _ => unreachable!(),
        }
        assert!(post_fn(&fixture, &post).specific.is_some(), "{invalid}");
        let polls = AtomicUsize::new(0);
        assert!(
            worker
                .run(&fixture.env, async {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert_eq!(polls.load(Ordering::SeqCst), 0, "{invalid}");
        assert!(matches!(fixture.record().phase, Phase::Failed { .. }));
    }
}

fn post_fn(fixture: &Fixture, input: &Input) -> Output {
    post(&fixture.runtime, &fixture.owner, input).unwrap()
}

#[test]
fn repairs_are_bounded_per_turn_and_never_block_observe_only_stop() {
    let fixture = Fixture::new();
    let mut input = fixture.input("stop");
    for decision in ["block", "block", "allow"] {
        assert_eq!(
            stop(&fixture.runtime, &fixture.owner, &input, &fixture.env)
                .unwrap()
                .decision,
            Some(decision)
        );
    }
    input.prompt_id = Some("new-turn".into());
    assert_eq!(
        stop(&fixture.runtime, &fixture.owner, &input, &fixture.env)
            .unwrap()
            .decision,
        Some("block")
    );
    input.reason = Some("shutdown".into());
    assert!(
        stop(&fixture.runtime, &fixture.owner, &input, &fixture.env)
            .unwrap()
            .decision
            .is_none()
    );
}

#[test]
fn background_request_requires_native_mode_and_no_positive_timeout() {
    let fixture = Fixture::new();
    for input in [
        serde_json::json!({"command":"clank grok-wait"}),
        serde_json::json!({"command":"clank grok-wait", "block_until_ms":100, "background":true}),
        serde_json::json!({"command":"clank grok-wait", "block_until_ms":0, "timeout":1000}),
    ] {
        let mut pre_input = fixture.input("pre_tool_use");
        pre_input.tool_input = input;
        assert_eq!(
            pre(&fixture.runtime, &fixture.owner, &pre_input, &fixture.env)
                .unwrap()
                .decision,
            Some("deny")
        );
        assert!(
            fixture
                .runtime
                .read(&fixture.owner.key())
                .unwrap()
                .is_none()
        );
    }
    for input in [
        serde_json::json!({"is_background":true}),
        serde_json::json!({"background":true}),
        serde_json::json!({"block_until_ms":0}),
        serde_json::json!({"timeout":0}),
    ] {
        assert!(background(&input));
    }
}

#[test]
fn unrelated_tasks_manual_waits_and_subagents_are_not_automatic_proof() {
    let fixture = Fixture::new();
    for command in [
        "clank wait",
        "echo clank grok-wait",
        "clank grok-wait; echo done",
        "monitor clank grok-wait",
    ] {
        let mut input = fixture.input("pre_tool_use");
        input.tool_input["command"] = command.into();
        assert!(
            pre(&fixture.runtime, &fixture.owner, &input, &fixture.env)
                .unwrap()
                .specific
                .is_none()
        );
    }
    let mut input = fixture.input("pre_tool_use");
    input.subagent_type = Some("explore".into());
    assert!(
        handle(&fixture.runtime, &input, &fixture.owner.repo, &fixture.env)
            .unwrap()
            .specific
            .is_none()
    );
    assert!(
        fixture
            .runtime
            .read(&fixture.owner.key())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn repeated_turns_do_not_spend_stop_repairs_or_accumulate_runtime_slots() {
    let fixture = Fixture::new();
    for turn in 0..12 {
        let mut input = fixture.input("pre_tool_use");
        input.prompt_id = Some(format!("turn-{turn}"));
        let output = pre(&fixture.runtime, &fixture.owner, &input, &fixture.env).unwrap();
        input.tool_input = output.specific.unwrap().updated_input.unwrap();
        let worker = fixture.worker(&input);
        let mut post = fixture.registration(&input);
        post.prompt_id = input.prompt_id.clone();
        post_fn(&fixture, &post);
        worker.run(&fixture.env, async { Ok(()) }).await.unwrap();
        let mut stop_input = fixture.input("stop");
        stop_input.prompt_id = input.prompt_id;
        assert!(
            stop(&fixture.runtime, &fixture.owner, &stop_input, &fixture.env)
                .unwrap()
                .decision
                .is_none()
        );
        assert_eq!(fixture.record().repairs, 0);
    }
    let prefix = format!("g_{}.", fixture.owner.key());
    let count = std::fs::read_dir(&fixture.runtime.root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .count();
    assert_eq!(count, 3, "one record and two locks, no per-turn files");
}

#[tokio::test]
async fn ownership_and_file_shape_fail_closed() {
    let fixture = Fixture::new();
    fixture.offer();
    let other = Stated::saying(&[(crate::agent_env::ENV_GROK_SESSION, "foreign")]);
    assert!(
        Worker::start(
            &fixture.runtime,
            fixture.owner.key(),
            fixture.record().generation,
            &other
        )
        .is_err()
    );
    assert!(fixture.runtime.read("../escape").is_err());
    let path = fixture.runtime.path(&fixture.owner.key(), "json").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(fixture.owner.repo.join(".clank/config.json"), &path).unwrap();
    assert!(fixture.runtime.read(&fixture.owner.key()).is_err());
}

#[test]
fn missing_native_identity_or_turn_is_not_compatibility_support() {
    assert!(
        serde_json::from_value::<Input>(
            serde_json::json!({"hook_event_name":"Stop", "session_id":SESSION,"cwd":"/tmp"})
        )
        .is_err()
    );
    let fixture = Fixture::new();
    let mut input = fixture.input("pre_tool_use");
    input.prompt_id = None;
    assert!(pre(&fixture.runtime, &fixture.owner, &input, &fixture.env).is_err());
    let cfg: AgentConfig = load_agent_config(&fixture.owner.repo, &fixture.owner.label)
        .unwrap()
        .unwrap();
    assert_eq!(cfg.session.unwrap().id, fixture.owner.session);
}

#[test]
fn releasing_a_lease_does_not_wait_for_an_inherited_descriptor_to_close() {
    let fixture = Fixture::new();
    let lease = fixture
        .runtime
        .lock(&fixture.owner.key(), "lease")
        .unwrap()
        .unwrap();
    let inherited = lease.0.try_clone().unwrap();
    assert!(
        fixture
            .runtime
            .lock(&fixture.owner.key(), "lease")
            .unwrap()
            .is_none()
    );
    drop(lease);
    let successor = fixture.runtime.lock(&fixture.owner.key(), "lease").unwrap();
    assert!(
        successor.is_some(),
        "a fork/dup holder must not extend the released owner's lease"
    );
    drop(inherited);
}

#[test]
fn a_repo_override_cannot_relocate_native_session_binding() {
    let fixture = Fixture::new();
    let mut input = fixture.input("session_start");
    input.cwd = std::path::PathBuf::from("/private/tmp");
    assert!(
        handle(&fixture.runtime, &input, &fixture.owner.repo, &fixture.env)
            .unwrap_err()
            .to_string()
            .contains("across repositories")
    );
    assert!(binding_matches(
        &fixture.owner.repo,
        &fixture.owner.label,
        Tool::Grok,
        SESSION
    ));
}

#[tokio::test]
async fn cancellation_after_work_selection_is_fenced_at_the_output_boundary() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    let worker = fixture.worker(&offer);
    fixture.confirm(&offer);
    worker.run(&fixture.env, async { Ok(()) }).await.unwrap();
    worker.authorize_delivery(&fixture.env).unwrap();
    fixture.runtime.cancel(&fixture.owner).unwrap();
    assert!(
        worker
            .authorize_delivery(&fixture.env)
            .unwrap_err()
            .to_string()
            .contains("before output")
    );
}

#[tokio::test(start_paused = true)]
async fn a_missing_registration_has_one_startup_deadline_and_never_polls_work() {
    let fixture = Fixture::new();
    let offer = fixture.offer();
    let worker = fixture.worker(&offer);
    let polls = AtomicUsize::new(0);
    let error = worker
        .run(&fixture.env, async {
            polls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("within 30 seconds"));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lifecycle_cancellation_closes_registration_without_clearing_work() {
    for event in ["session_end", "stop_cancelled", "stop_failure"] {
        let fixture = Fixture::new();
        let offer = fixture.offer();
        let worker = fixture.worker(&offer);
        let delivery = worker.run(&fixture.env, std::future::pending::<Result<()>>());
        tokio::pin!(delivery);
        assert!(matches!(futures_util::poll!(&mut delivery), Poll::Pending));
        handle(
            &fixture.runtime,
            &fixture.input(event),
            &fixture.owner.repo,
            &fixture.env,
        )
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut delivery)
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert!(binding_matches(
            &fixture.owner.repo,
            &fixture.owner.label,
            Tool::Grok,
            SESSION
        ));
    }
}

#[cfg(test)]
mod real_wait {
    use super::*;
    use clank_core::wait::WaitItem;

    #[tokio::test]
    async fn native_handoff_delivers_real_inbox_work_without_acknowledging_it() {
        let fixture = Fixture::new();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "--no-verify",
                "-q",
                "-m",
                "fixture",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&fixture.owner.repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let handle = crate::inbox::deliver(
            &fixture.owner.repo,
            &fixture.owner.label,
            None,
            "2026-10-03T00:00:00Z",
            "handoff fixture work",
        )
        .unwrap();
        let offer = fixture.offer();
        let worker = fixture.worker(&offer);
        let args = WaitArgs {
            repo: Some(fixture.owner.repo.clone()),
            author: Some("grok".into()),
            die_with_owner: false,
            deliver_to_session: None,
            json: false,
            events: vec![],
            r#for: None,
            peek: false,
            no_cache: true,
            poll: false,
            no_poll: true,
        };
        let delivery = worker.run(
            &fixture.env,
            super::super::super::wait::automatic_grok_items(args, fixture.owner.session.clone()),
        );
        tokio::pin!(delivery);
        assert!(matches!(futures_util::poll!(&mut delivery), Poll::Pending));
        assert!(
            crate::inbox::pending(&fixture.owner.repo, &fixture.owner.label)
                .iter()
                .any(|item| matches!(item, WaitItem::Message { handle: id, .. } if id == &handle))
        );
        fixture.confirm(&offer);
        let items = tokio::time::timeout(std::time::Duration::from_secs(10), &mut delivery)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            items
                .iter()
                .any(|item| matches!(item, WaitItem::Message { handle: id, .. } if id == &handle))
        );
        assert!(
            crate::inbox::pending(&fixture.owner.repo, &fixture.owner.label)
                .iter()
                .any(|item| matches!(item, WaitItem::Message { handle: id, .. } if id == &handle)),
            "delivery is not acknowledgment"
        );
    }
}

#[test]
fn existing_manual_task_requires_an_explicit_migration_choice() {
    let f = Fixture::new();
    let mut input = f.input("stop");
    input.background_tasks = Some(vec![Task {
        id: "manual-1".into(),
        kind: "shell".into(),
        status: "running".into(),
        command: Some("exec /usr/local/bin/clank wait --repo /somewhere".into()),
    }]);
    let out = handle(&f.runtime, &input, &f.owner.repo, &f.env).unwrap();
    assert_eq!(out.decision, Some("block"));
    assert!(out.reason.unwrap().contains("manual-1"));
    let arm = f.input("pre_tool_use");
    let denied = handle(&f.runtime, &arm, &f.owner.repo, &f.env).unwrap();
    assert_eq!(denied.decision, Some("deny"));
    assert!(denied.reason.unwrap().contains("clank auto off"));
    input.background_tasks = Some(vec![]);
    let repair = handle(&f.runtime, &input, &f.owner.repo, &f.env).unwrap();
    assert_eq!(repair.reason.as_deref(), Some(ARM));
    assert!(
        handle(&f.runtime, &arm, &f.owner.repo, &f.env)
            .unwrap()
            .specific
            .unwrap()
            .updated_input
            .is_some()
    );
}

#[test]
fn absent_task_snapshot_cannot_clear_known_manual_task() {
    let f = Fixture::new();
    let mut input = f.input("stop");
    input.background_tasks = Some(vec![Task {
        id: "manual-1".into(),
        kind: "shell".into(),
        status: "running".into(),
        command: Some("clank wait".into()),
    }]);
    handle(&f.runtime, &input, &f.owner.repo, &f.env).unwrap();
    input.background_tasks = None;
    let repair = handle(&f.runtime, &input, &f.owner.repo, &f.env).unwrap();
    assert!(repair.reason.unwrap().contains("manual-1"));
    assert_eq!(f.record().manual_task.as_deref(), Some("manual-1"));
    assert!(manual_wait_command(
        "exec '/path with spaces/clank' wait --json"
    ));
    assert!(!manual_wait_command("echo clank wait"));
    assert!(!manual_wait_command("clank grok-wait"));
}

#[test]
fn diagnosis_reads_phase_and_lease_without_claiming_model_receipt_or_mutating_state() {
    let f = Fixture::new();
    assert!(
        diagnose(&f.owner.repo, &f.owner.label, None)
            .unwrap()
            .starts_with("unbound")
    );
    assert!(
        diagnose(&f.owner.repo, &f.owner.label, Some(&f.owner.session))
            .unwrap()
            .contains("no native handoff")
    );
    let offer = f.offer();
    assert!(
        diagnose(&f.owner.repo, &f.owner.label, Some(&f.owner.session))
            .unwrap()
            .contains("confirmation pending")
    );
    let post = f.registration(&offer);
    handle(&f.runtime, &post, &f.owner.repo, &f.env).unwrap();
    let path = f.runtime.path(&f.owner.key(), "json").unwrap();
    let before = std::fs::read(&path).unwrap();
    let lease = f.runtime.lock(&f.owner.key(), "lease").unwrap().unwrap();
    let report = diagnose(&f.owner.repo, &f.owner.label, Some(&f.owner.session)).unwrap();
    assert!(report.contains("lease held"));
    assert!(report.contains("unverified"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(lease);
    assert!(
        diagnose(&f.owner.repo, &f.owner.label, Some(&f.owner.session))
            .unwrap()
            .contains("no live")
    );
    let mut record = f.record();
    record.phase = Phase::Delivered {
        task: "task-1".into(),
    };
    f.runtime.write(&f.owner.key(), &record).unwrap();
    assert!(
        diagnose(&f.owner.repo, &f.owner.label, Some(&f.owner.session))
            .unwrap()
            .contains("reaction are unverified")
    );
}
