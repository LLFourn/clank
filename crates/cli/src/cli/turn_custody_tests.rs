use super::*;
use crate::cli::turn_wire::WireVerdict;
use clank_core::turn_policy::{AttendRequest, TaskSnapshot, TurnEndReport, TurnPlace};

/// A check-in falling due for an agent unbound since it was armed is
/// retired with the binding, not left due: a deadline already passed would
/// wake the monitor's loop again at once, for ever.
#[test]
fn unbinding_an_attended_agent_removes_its_due_timer() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[], Ok(()));
    attend_name(&mut turns, "claude", "session-one", &g);
    turns
        .answer(
            &author,
            1,
            &stop(Tool::Claude, "session-one", running_subagent(), None),
            true,
            &g,
        )
        .unwrap();
    let due = turns.next_deadline().expect("armed");
    crate::agent_store::release_binding(repo.path(), &author, &sid("session-one")).unwrap();
    let unbound = gathered(repo.path(), "claude", &[], Ok(())).at(due);
    assert!(turns.check_in(&author, &unbound).is_none());
    assert_eq!(
        turns.next_deadline(),
        None,
        "an unbound agent must not keep an already-due timer spinning"
    );
}

const SUBAGENT: &str = "capture-probe sleeper";
const MINUTE: Ms = 60_000;

fn label(name: &str) -> AgentLabel {
    AgentLabel::parse(name).unwrap()
}

fn sid(s: &str) -> SessionId {
    SessionId::parse(s).unwrap()
}

/// A repo whose master `name` runs `tool`, bound to `session`, auto on.
fn repo_with(name: &str, tool: Tool, session: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    roster(dir.path(), name, tool, session);
    dir
}

fn roster(repo: &Path, name: &str, tool: Tool, session: &str) {
    let author = label(name);
    crate::cli::agent::add_repo_roster_agent(
        repo,
        &author,
        crate::cli::teams_config::AgentDescription {
            tool,
            launch: None,
            initial_prompt: None,
        },
        crate::cli::teams_config::RosterRole::Commit,
    )
    .unwrap();
    crate::cli::agent::set_repo_master(repo, &author).unwrap();
    crate::agent_store::bind_session_to_agent(repo, &author, tool, &sid(session)).unwrap();
    crate::agent_store::set_auto_mode(repo, &author, AutoMode::On).unwrap();
}

fn message(handle: &str) -> clank_core::wait::WaitItem {
    clank_core::wait::WaitItem::Message {
        presentation: None,
        requires_reply: false,
        reply_route: None,
        handle: handle.into(),
        from: None,
        repo: None,
        subject: "Rebase please".into(),
        body: "onto master".into(),
        selector: Some(format!("messages@{handle}")),
    }
}

fn gathered(
    repo: &Path,
    who: &str,
    owed: &[clank_core::wait::WaitItem],
    route: Result<(), String>,
) -> Gathered {
    Gathered::of(repo, &label(who), Some((owed, None)), route)
}

fn running_subagent() -> TaskSnapshot {
    TaskSnapshot::Known(vec![
        serde_json::from_value(serde_json::json!({
            "id": "a25eee2eef81caf3e", "type": "subagent", "status": "running",
            "description": SUBAGENT, "agent_type": "general-purpose",
        }))
        .unwrap(),
    ])
}

fn stop(tool: Tool, session: &str, tasks: TaskSnapshot, position: Option<u64>) -> Report {
    Report::TurnEnd {
        stop: TurnEndReport {
            tool,
            session: sid(session),
            place: TurnPlace {
                turn: position.map(|p| format!("turn-{p}")),
                position,
                human_turn: false,
            },
            tasks,
        },
        deadline: u64::MAX,
        questions: Vec::new(),
    }
}

fn attend_name(turns: &mut Turns, who: &str, session: &str, g: &Gathered) -> String {
    turns
        .attend(
            &label(who),
            AttendRequest::Attend {
                session: sid(session),
                selector: Selector::Name(SUBAGENT.into()),
                desc: None,
                expect: 10 * MINUTE,
                shell: None,
            },
            g,
        )
        .unwrap()
}

fn silent(why: SilentReason) -> WireVerdict {
    WireVerdict::Silent { why }
}

/// Attend a subagent by name, and the Stop that lists it is quiet: the
/// whole path through the monitor's custody, persisted beside the agent
/// and shown in its projection.
#[test]
fn a_subagent_attended_by_name_quiets_the_stop_that_lists_it() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[], Ok(()));
    let said = attend_name(&mut turns, "claude", "session-one", &g);
    assert!(said.contains("from your next turn-end"), "{said}");
    assert!(said.contains("after 10m"), "{said}");
    let (shown, _) = turns.projection(&label("claude"), &g);
    assert!(shown.as_ref().is_some_and(|a| a.pending), "{shown:?}");

    let frame = turns
        .answer(
            &label("claude"),
            1,
            &stop(Tool::Claude, "session-one", running_subagent(), None),
            true,
            &g,
        )
        .unwrap();
    assert_eq!(frame.verdict, silent(SilentReason::AttendingBackgroundTask));
    let (shown, _) = turns.projection(&label("claude"), &g);
    let shown = shown.expect("attending");
    assert!(shown.subagent && !shown.pending, "{shown:?}");
    assert_eq!(shown.task, "a25eee2eef81caf3e");
    assert_eq!(shown.subject(), SUBAGENT);
    assert!(
        crate::agent_store::agents_root(repo.path())
            .join("claude/turn.json")
            .exists(),
        "the monitor keeps it beside the agent"
    );
}

/// A restarted monitor keeps the attendance with its original deadline,
/// and it quiets nothing until the next Stop proves it again.
#[test]
fn a_restart_keeps_the_deadline_and_proves_nothing_until_the_next_stop() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[], Ok(()));
    attend_name(&mut turns, "claude", "session-one", &g);
    turns
        .answer(
            &author,
            1,
            &stop(Tool::Claude, "session-one", running_subagent(), None),
            true,
            &g,
        )
        .unwrap();
    let due = turns.next_deadline().expect("a check-in is owed");

    let mut back = Turns::new(repo.path().to_path_buf());
    assert_eq!(back.recovery_quiet(&author, &g), None);
    assert_eq!(back.next_deadline(), Some(due));
    let frame = back
        .answer(
            &author,
            2,
            &stop(Tool::Claude, "session-one", running_subagent(), None),
            true,
            &g,
        )
        .unwrap();
    assert_eq!(frame.verdict, silent(SilentReason::AttendingBackgroundTask));
    assert_eq!(back.recovery_quiet(&author, &g), Some(Quiet::Attending));
}

/// The drive gate as the monitor holds it: auto off is a silence, an
/// unresolvable role is a stated failure that arms nothing, and auto off
/// outranks it.
#[test]
fn the_drive_gate_answers_before_anything_is_presented() {
    let repo = repo_with("codex", Tool::Codex, "sess-gate");
    let author = label("codex");
    let report = stop(Tool::Codex, "sess-gate", TaskSnapshot::Unavailable, None);
    let mut turns = Turns::new(repo.path().to_path_buf());
    let owed = [message("1")];

    crate::agent_store::set_auto_mode(repo.path(), &author, AutoMode::Off).unwrap();
    let frame = turns
        .answer(
            &author,
            1,
            &report,
            true,
            &gathered(repo.path(), "codex", &owed, Ok(())),
        )
        .unwrap();
    assert_eq!(frame.verdict, silent(SilentReason::AutoOff));

    // No master: the roster cannot say who this is.
    std::fs::write(
        repo.path().join(".clank/config.json"),
        r#"{"agents":{"codex":{"tool":"codex","role":"commit"}}}"#,
    )
    .unwrap();
    let frame = turns
        .answer(
            &author,
            2,
            &report,
            true,
            &gathered(repo.path(), "codex", &owed, Ok(())),
        )
        .unwrap();
    assert_eq!(
        frame.verdict,
        silent(SilentReason::AutoOff),
        "auto off outranks it"
    );

    crate::agent_store::set_auto_mode(repo.path(), &author, AutoMode::On).unwrap();
    let frame = turns
        .answer(
            &author,
            3,
            &report,
            true,
            &gathered(repo.path(), "codex", &owed, Ok(())),
        )
        .unwrap();
    let WireVerdict::Diagnostic { message } = frame.verdict else {
        panic!("{frame:?}");
    };
    assert!(message.contains("role"), "names the failure: {message}");
    assert!(
        message.contains("arming no wait"),
        "says what it did not do: {message}"
    );
}

/// Owed work is presented once, rendered by the monitor for the agent's
/// role, and the live listing adds the recipe that ends the loop.
#[test]
fn owed_work_is_presented_with_the_text_the_hook_frames() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let owed = [message("7")];
    let g = gathered(repo.path(), "claude", &owed, Ok(()));
    let frame = turns
        .answer(
            &author,
            1,
            &stop(Tool::Claude, "session-one", running_subagent(), None),
            true,
            &g,
        )
        .unwrap();
    assert!(frame.presentation().is_some(), "{frame:?}");
    let text = frame.rendered.expect("rendered");
    assert!(
        text.starts_with("Clank wait returned work for `claude` (master). Items:"),
        "{text}"
    );
    assert!(text.contains("message: 7"), "{text}");
    assert!(text.contains("`clank attend \"<name>\"`"), "{text}");

    let parked = turns
        .answer(
            &author,
            2,
            &stop(
                Tool::Claude,
                "session-one",
                TaskSnapshot::Known(Vec::new()),
                None,
            ),
            true,
            &g,
        )
        .unwrap();
    assert_eq!(
        parked.verdict,
        WireVerdict::Park,
        "the same work in flight to another hook is not handed out twice"
    );
}

/// Repeated deliveries are counted by delivery, each against the Stop it
/// came from: the same Stop seen again is not counted, a failed emit
/// counts nothing, and the fourth escalates with each item's duty.
#[test]
fn repeated_delivery_is_counted_by_delivery_and_escalates_with_its_duties() {
    let repo = repo_with("codex", Tool::Codex, "sess-repeat");
    let author = label("codex");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let owed = [message("9")];
    let g = gathered(repo.path(), "codex", &owed, Ok(()));
    let mut conn = 0;
    let mut deliver = |turns: &mut Turns, position: u64, ok: bool| {
        conn += 1;
        let frame = turns
            .answer(
                &author,
                conn,
                &stop(
                    Tool::Codex,
                    "sess-repeat",
                    TaskSnapshot::Unavailable,
                    Some(position),
                ),
                true,
                &g,
            )
            .unwrap();
        let presentation = frame.presentation().expect("delivered").clone();
        let result = match ok {
            true => TurnResult::Delivered { presentation },
            false => TurnResult::EmitFailed {
                presentation,
                why: "broken pipe".into(),
            },
        };
        turns.result(&author, conn, result, &g).unwrap();
        frame.rendered.unwrap()
    };
    let count = |turns: &Turns| {
        turns
            .state(&author)
            .and_then(|s| s.repeat.as_ref())
            .map(|r| r.count)
    };
    for n in 1..=3 {
        let text = deliver(&mut turns, n, true);
        assert!(!text.contains("times unchanged"), "{text}");
        assert_eq!(count(&turns), Some(n as u32));
    }
    deliver(&mut turns, 3, true);
    assert_eq!(count(&turns), Some(3), "the same Stop, seen again");
    deliver(&mut turns, 4, false);
    assert_eq!(
        count(&turns),
        Some(3),
        "a continuation that never reached the agent"
    );
    let text = deliver(&mut turns, 5, true);
    assert!(text.contains("4 times unchanged"), "{text}");
    assert!(text.contains("`clank events ack messages@9`"), "{text}");
    let (_, repeated) = turns.projection(&author, &g);
    assert_eq!(repeated, Some(4));
}

/// The projection shows a count only once it escalates.
#[test]
fn a_count_below_the_threshold_is_not_shown() {
    let repo = repo_with("codex", Tool::Codex, "sess-below");
    let author = label("codex");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "codex", &[message("1")], Ok(()));
    let frame = turns
        .answer(
            &author,
            1,
            &stop(Tool::Codex, "sess-below", TaskSnapshot::Unavailable, None),
            true,
            &g,
        )
        .unwrap();
    let presentation = frame.presentation().unwrap().clone();
    turns
        .result(&author, 1, TurnResult::Delivered { presentation }, &g)
        .unwrap();
    assert_eq!(turns.projection(&author, &g).1, None);
}

/// Only the connection that carried a presentation can answer for it.
/// A result from any other connection — an older hook's, a client's —
/// commits nothing, and the carrier's own result still commits once.
#[test]
fn a_result_from_another_connection_is_not_credited() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[message("1")], Ok(()));
    let frame = turns
        .answer(
            &author,
            10,
            &stop(
                Tool::Claude,
                "session-one",
                TaskSnapshot::Known(Vec::new()),
                None,
            ),
            true,
            &g,
        )
        .unwrap();
    let presentation = frame.presentation().unwrap().clone();
    assert_eq!(turns.carrier(10), Some(&author));
    assert_eq!(turns.carrier(11), None);
    assert!(
        turns
            .result(
                &author,
                11,
                TurnResult::Delivered {
                    presentation: presentation.clone()
                },
                &g
            )
            .is_err()
    );
    assert_eq!(turns.carrier(10), Some(&author), "still in flight");
    assert!(
        turns
            .result(
                &author,
                10,
                TurnResult::Delivered {
                    presentation: presentation.clone()
                },
                &g
            )
            .unwrap()
            .is_some(),
        "the carrier's result commits"
    );
    assert!(
        turns
            .result(&author, 10, TurnResult::Delivered { presentation }, &g)
            .is_err(),
        "and only once"
    );
}

/// A carrier that closes without a result failed, terminally: the work
/// stays owed and is presented fresh, and a late result for the failed
/// presentation commits nothing.
#[test]
fn a_closed_carrier_fails_its_presentation_and_the_work_is_presented_fresh() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[message("1")], Ok(()));
    let report = stop(
        Tool::Claude,
        "session-one",
        TaskSnapshot::Known(Vec::new()),
        None,
    );
    let first = turns.answer(&author, 1, &report, true, &g).unwrap();
    let failed = first.presentation().unwrap().clone();
    turns.closed(&author, 1, &g);
    assert_eq!(turns.carrier(1), None);
    let again = turns.answer(&author, 2, &report, true, &g).unwrap();
    assert_ne!(again.presentation(), Some(&failed), "presented fresh");
    assert!(
        turns
            .result(
                &author,
                1,
                TurnResult::Delivered {
                    presentation: failed
                },
                &g
            )
            .is_err()
    );
}

/// The check-in route: with none, an attendance quiets nothing and the
/// turn-end parks; one lost after the silence makes the check-in
/// undeliverable, which is shown, and recovery treats the agent as any
/// other past the deadline.
#[test]
fn a_silence_needs_a_route_and_a_lost_one_is_recorded() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let routeless = gathered(
        repo.path(),
        "claude",
        &[],
        Err("its version is not known yet".into()),
    );
    attend_name(&mut turns, "claude", "session-one", &routeless);
    let report = stop(Tool::Claude, "session-one", running_subagent(), None);
    let frame = turns.answer(&author, 1, &report, true, &routeless).unwrap();
    assert_eq!(frame.verdict, WireVerdict::Park);
    assert_eq!(
        turns.next_deadline(),
        None,
        "nothing quieted, so no check-in is owed"
    );

    let g = gathered(repo.path(), "claude", &[], Ok(()));
    let frame = turns.answer(&author, 2, &report, true, &g).unwrap();
    assert_eq!(frame.verdict, silent(SilentReason::AttendingBackgroundTask));
    let due = turns.next_deadline().expect("armed");
    let at_due = gathered(repo.path(), "claude", &[], Ok(())).at(due);
    let check_in = turns.check_in(&author, &at_due).expect("owed");
    assert_eq!(check_in.session, sid("session-one"));
    assert!(check_in.text.contains("check-in"), "{}", check_in.text);
    assert!(
        check_in
            .text
            .contains(&format!("clank attend '{SUBAGENT}' --expect 20m")),
        "{}",
        check_in.text
    );
    turns.check_in_failed(
        &author,
        check_in.check_in.clone(),
        "no pane was found".into(),
        &at_due,
    );
    let (shown, _) = turns.projection(&author, &at_due);
    assert_eq!(
        shown.and_then(|a| a.check_in).as_deref(),
        Some("check-in undeliverable: no pane was found")
    );
    assert_eq!(turns.recovery_quiet(&author, &at_due), None);
    assert!(
        turns.check_in(&author, &at_due).is_none(),
        "spent, not retried"
    );
}

/// The deadline verdict each tool takes: a heartbeat for Claude and
/// Codex, a renewal for OpenCode's plugin, a stated failure for Grok.
#[test]
fn a_parked_report_at_its_deadline_re_arms_without_spinning_opencode() {
    for tool in [Tool::Claude, Tool::Codex] {
        let frame = Turns::deadline_frame(tool);
        assert_eq!(frame.verdict, WireVerdict::Notify, "{tool:?}");
        assert_eq!(
            frame.rendered.as_deref(),
            Some(super::super::wake_render::REARM_REASON)
        );
    }
    assert_eq!(
        Turns::deadline_frame(Tool::OpenCode).verdict,
        WireVerdict::RenewWait
    );
    assert!(matches!(
        Turns::deadline_frame(Tool::Grok).verdict,
        WireVerdict::Diagnostic { .. }
    ));
}

/// What the Stop proves about the agent's questions is applied by the
/// monitor: the question its turn asked is over.
#[test]
fn a_stops_question_evidence_ends_the_question() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let dir = crate::agent_store::agents_root(repo.path()).join("claude");
    let key = super::super::question_state::Key {
        session: "session-one".into(),
        prompt: "p1".into(),
        tool_use: "t1".into(),
    };
    super::super::question_state::observe(
        &dir,
        [super::super::question_state::Observation::Asked {
            key,
            header: "Which?".into(),
            input: "h".into(),
        }],
    );
    assert!(super::super::question_state::asking(&dir, "session-one").is_some());
    let turns = Turns::new(repo.path().to_path_buf());
    let Report::TurnEnd { stop, deadline, .. } = stop(
        Tool::Claude,
        "session-one",
        TaskSnapshot::Known(Vec::new()),
        None,
    ) else {
        unreachable!()
    };
    turns.observe_questions(
        &author,
        &Report::TurnEnd {
            stop,
            deadline,
            questions: vec![super::super::question_state::Observation::TurnOver {
                session: "session-one".into(),
                prompt: "p1".into(),
            }],
        },
    );
    assert!(super::super::question_state::asking(&dir, "session-one").is_none());
}

/// The receipt fixture's agent, on the roster so its role resolves.
fn receipt_fixture(tool: Tool) -> super::super::wake_receipt::fixtures::Fixture {
    let fixture = super::super::wake_receipt::fixtures::Fixture::new();
    roster(
        &fixture.repo,
        fixture.author.as_str(),
        tool,
        &fixture.session,
    );
    fixture
}

fn present_receipt(
    fixture: &super::super::wake_receipt::fixtures::Fixture,
    turns: &mut Turns,
    tool: Tool,
    conn: u64,
) -> (Frame, Gathered) {
    let g = Gathered::of(
        &fixture.repo,
        &fixture.author,
        Some((std::slice::from_ref(&fixture.item), Some(&fixture.cwd))),
        Ok(()),
    );
    let report = match tool {
        Tool::Claude => stop(
            tool,
            &fixture.session,
            TaskSnapshot::Known(Vec::new()),
            None,
        ),
        _ => stop(tool, &fixture.session, TaskSnapshot::Unavailable, None),
    };
    let frame = turns
        .answer(&fixture.author, conn, &report, true, &g)
        .unwrap();
    (frame, g)
}

/// A presentation's receipt commits on its `Delivered`, after the hook's
/// flush, on every adapter, and leaves the work owed; a failed emit or a
/// closed carrier commits nothing.
#[tokio::test]
async fn a_receipt_follows_a_delivered_presentation_and_nothing_else() {
    for tool in [Tool::Codex, Tool::Claude, Tool::OpenCode] {
        let fixture = receipt_fixture(tool);
        let mut turns = Turns::new(fixture.repo.clone());
        let (frame, g) = present_receipt(&fixture, &mut turns, tool, 1);
        assert!(
            frame
                .rendered
                .as_deref()
                .is_some_and(|t| t.contains("do this")),
            "{frame:?}"
        );
        assert!(
            fixture.rows()[0].woken_at.is_none(),
            "choosing it is not delivering it"
        );
        std::fs::write(fixture.marker(), "flushed").unwrap();
        let commit = turns
            .result(
                &fixture.author,
                1,
                TurnResult::Delivered {
                    presentation: frame.presentation().unwrap().clone(),
                },
                &g,
            )
            .unwrap()
            .expect("a delivery commits");
        commit.run(&fixture.repo).await;
        assert!(
            fixture.output.exists(),
            "{tool:?}: the callback ran after the flush"
        );
        assert!(fixture.rows()[0].woken_at.is_some());
        assert!(!fixture.rows()[0].acked);
        assert!(fixture.rows()[0].reply.is_none());
    }
    for close in [false, true] {
        let fixture = receipt_fixture(Tool::Codex);
        let mut turns = Turns::new(fixture.repo.clone());
        let (frame, g) = present_receipt(&fixture, &mut turns, Tool::Codex, 1);
        if close {
            turns.closed(&fixture.author, 1, &g);
        } else {
            let failed = turns
                .result(
                    &fixture.author,
                    1,
                    TurnResult::EmitFailed {
                        presentation: frame.presentation().unwrap().clone(),
                        why: "broken pipe".into(),
                    },
                    &g,
                )
                .unwrap();
            assert!(failed.is_none());
        }
        assert!(!fixture.output.exists());
        assert!(fixture.rows()[0].woken_at.is_none());
    }
}

/// A binding replaced between the flush and the commit earns neither
/// delivery evidence nor a callback.
#[tokio::test]
async fn a_binding_replaced_before_the_commit_earns_nothing() {
    let fixture = receipt_fixture(Tool::Codex);
    let mut turns = Turns::new(fixture.repo.clone());
    let (frame, g) = present_receipt(&fixture, &mut turns, Tool::Codex, 1);
    let commit = turns
        .result(
            &fixture.author,
            1,
            TurnResult::Delivered {
                presentation: frame.presentation().unwrap().clone(),
            },
            &g,
        )
        .unwrap()
        .unwrap();
    crate::agent_store::bind_session_to_agent(
        &fixture.repo,
        &fixture.author,
        Tool::Codex,
        &sid("replacement-at-flush"),
    )
    .unwrap();
    std::fs::write(fixture.marker(), "flushed").unwrap();
    commit.run(&fixture.repo).await;
    assert!(fixture.rows()[0].woken_at.is_none());
    assert!(!fixture.output.exists());
}

/// Delivery evidence does not depend on a receipt command being set up.
#[tokio::test]
async fn presentation_is_independent_of_receipt_configuration() {
    let mut fixture = receipt_fixture(Tool::Codex);
    fixture.update_source(|source| source.receipt_command = None);
    let mut turns = Turns::new(fixture.repo.clone());
    let (frame, g) = present_receipt(&fixture, &mut turns, Tool::Codex, 1);
    turns
        .result(
            &fixture.author,
            1,
            TurnResult::Delivered {
                presentation: frame.presentation().unwrap().clone(),
            },
            &g,
        )
        .unwrap()
        .unwrap()
        .run(&fixture.repo)
        .await;
    assert!(fixture.rows()[0].woken_at.is_some());
    assert!(!fixture.output.exists());
}

/// A session start presents its owed work once, uncounted; its receipt
/// follows only where the context reaches the model, which is Claude's.
#[tokio::test]
async fn a_session_starts_receipt_follows_only_claudes_context() {
    for (tool, receipted) in [(Tool::Claude, true), (Tool::Codex, false)] {
        let fixture = receipt_fixture(tool);
        let mut turns = Turns::new(fixture.repo.clone());
        let g = Gathered::of(
            &fixture.repo,
            &fixture.author,
            Some((std::slice::from_ref(&fixture.item), Some(&fixture.cwd))),
            Ok(()),
        );
        let report = Report::SessionStart {
            tool,
            session: sid(&fixture.session),
            deadline: u64::MAX,
        };
        let frame = turns.answer(&fixture.author, 1, &report, true, &g).unwrap();
        std::fs::write(fixture.marker(), "flushed").unwrap();
        if let Some(commit) = turns
            .result(
                &fixture.author,
                1,
                TurnResult::Delivered {
                    presentation: frame.presentation().expect("presented").clone(),
                },
                &g,
            )
            .unwrap()
        {
            commit.run(&fixture.repo).await;
        }
        assert_eq!(fixture.output.exists(), receipted, "{tool:?}");
        assert_eq!(fixture.rows()[0].woken_at.is_some(), receipted, "{tool:?}");
        assert_eq!(
            turns.state(&fixture.author).and_then(|s| s.repeat.as_ref()),
            None,
            "a session start is not a repeat"
        );
    }
}

/// Alone, owed work wakes the agent with a notice and presents nothing,
/// whether or not that notice lands; once a monitor is back it presents
/// the same work once, and its receipt commits once.
#[tokio::test]
async fn alone_mode_presents_nothing_and_the_monitor_delivers_once_after() {
    let fixture = receipt_fixture(Tool::Codex);
    let owed = std::slice::from_ref(&fixture.item);
    let alone = Gathered::of(
        &fixture.repo,
        &fixture.author,
        Some((owed, None)),
        Err(String::new()),
    );
    let verdict = alone.alone_verdict(&sid(&fixture.session));
    assert_eq!(
        frame_of(verdict).verdict,
        WireVerdict::Notify,
        "owed work wakes the agent"
    );
    assert_eq!(
        alone.alone_verdict(&sid("someone-else-entirely")),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    assert!(
        fixture.rows()[0].woken_at.is_none(),
        "a notice is not a presentation"
    );
    assert!(!fixture.output.exists());

    let mut turns = Turns::new(fixture.repo.clone());
    let (frame, g) = present_receipt(&fixture, &mut turns, Tool::Codex, 1);
    std::fs::write(fixture.marker(), "flushed").unwrap();
    let presentation = frame.presentation().expect("the same work").clone();
    turns
        .result(
            &fixture.author,
            1,
            TurnResult::Delivered {
                presentation: presentation.clone(),
            },
            &g,
        )
        .unwrap()
        .unwrap()
        .run(&fixture.repo)
        .await;
    assert!(fixture.output.exists());
    let first = std::fs::read(&fixture.output).unwrap();
    std::fs::remove_file(&fixture.output).unwrap();
    assert!(
        turns
            .result(
                &fixture.author,
                1,
                TurnResult::Delivered { presentation },
                &g
            )
            .is_err(),
        "nothing is committed twice"
    );
    assert!(!fixture.output.exists());
    assert!(!first.is_empty());
}

/// Alone mode is never quiet on attendance, and still honours auto off
/// and a pending question.
#[test]
fn alone_mode_is_never_quiet_on_attendance() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[], Ok(()));
    attend_name(&mut turns, "claude", "session-one", &g);
    turns
        .answer(
            &author,
            1,
            &stop(Tool::Claude, "session-one", running_subagent(), None),
            true,
            &g,
        )
        .unwrap();
    let alone = gathered(repo.path(), "claude", &[message("1")], Err(String::new()));
    assert_eq!(
        alone.alone_verdict(&sid("session-one")),
        Verdict::Notify(clank_core::turn_policy::Notice::MonitorUnavailable)
    );
    crate::agent_store::set_auto_mode(repo.path(), &author, AutoMode::Off).unwrap();
    let off = gathered(repo.path(), "claude", &[message("1")], Err(String::new()));
    assert_eq!(
        off.alone_verdict(&sid("session-one")),
        Verdict::Silent(SilentWhy::AutoOff)
    );
}
/// A receipt callback that fails leaves the delivery evidence in place and
/// replaces only Claude's bounded diagnostic file.
#[tokio::test]
async fn a_failing_receipt_callback_keeps_the_evidence_and_bounds_its_diagnostic() {
    let mut fixture = receipt_fixture(Tool::Claude);
    fixture.update_source(|source| {
        source.receipt_command = Some(vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            "import sys; sys.stderr.write('x'*100000); sys.exit(7)".into(),
        ])
    });
    let mut turns = Turns::new(fixture.repo.clone());
    let (frame, g) = present_receipt(&fixture, &mut turns, Tool::Claude, 1);
    turns
        .result(
            &fixture.author,
            1,
            TurnResult::Delivered {
                presentation: frame.presentation().unwrap().clone(),
            },
            &g,
        )
        .unwrap()
        .unwrap()
        .run(&fixture.repo)
        .await;
    let diagnostic = std::fs::read(super::super::wake_receipt::diagnostic_path(
        &fixture.repo,
        &fixture.author,
    ))
    .unwrap();
    assert!(diagnostic.len() <= super::super::wake_receipt::DIAGNOSTIC_BYTES);
    assert!(String::from_utf8(diagnostic).unwrap().contains("exited"));
    assert!(
        fixture.rows()[0].woken_at.is_some(),
        "a callback failure cannot erase wake evidence"
    );
    assert!(!fixture.rows()[0].acked);
    assert!(fixture.rows()[0].reply.is_none());
}

/// The role a presentation is worded for is the roster's when it is made,
/// not when the hook first reported: a parked report re-stepped after a
/// roster change names the new role.
#[test]
fn a_presentation_is_worded_for_the_role_the_roster_gives_now() {
    let repo = repo_with("codex", Tool::Codex, "sess-role");
    let author = label("codex");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let report = stop(Tool::Codex, "sess-role", TaskSnapshot::Unavailable, None);
    let parked = turns
        .answer(
            &author,
            1,
            &report,
            true,
            &gathered(repo.path(), "codex", &[], Ok(())),
        )
        .unwrap();
    assert_eq!(parked.verdict, WireVerdict::Park);
    crate::cli::agent::add_repo_roster_agent(
        repo.path(),
        &label("other"),
        crate::cli::teams_config::AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
        crate::cli::teams_config::RosterRole::Commit,
    )
    .unwrap();
    crate::cli::agent::set_repo_master(repo.path(), &label("other")).unwrap();
    let frame = turns
        .answer(
            &author,
            1,
            &report,
            false,
            &gathered(repo.path(), "codex", &[message("1")], Ok(())),
        )
        .unwrap();
    let text = frame.rendered.expect("presented");
    assert!(
        text.starts_with("Clank wait returned work for `codex` (reviewer). Items:"),
        "{text}"
    );
}

/// Grok's native Stop is answered by the native rule, before any fold:
/// it needs no owed work, presents nothing, and holds no presentation.
#[test]
fn a_grok_handoff_is_answered_without_waiting_for_a_fold() {
    let repo = repo_with("grok", Tool::Grok, "session-one");
    let author = label("grok");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let unfolded = Gathered::of(repo.path(), &author, None, Ok(()));
    let handoff = |wait_running| Report::Handoff {
        handoff: clank_core::turn_policy::Handoff {
            session: sid("session-one"),
            wait_running,
            continued: false,
        },
        deadline: u64::MAX,
    };
    let frame = turns
        .answer(&author, 1, &handoff(false), true, &unfolded)
        .expect("answered unfolded");
    assert_eq!(frame.verdict, WireVerdict::Notify);
    assert_eq!(
        frame.rendered.as_deref(),
        Some(super::super::wake_render::ARM_WAIT)
    );
    let frame = turns
        .answer(&author, 2, &handoff(true), true, &unfolded)
        .unwrap();
    assert_eq!(frame.verdict, silent(SilentReason::WaitArmed));
    assert!(turns.state(&author).is_none_or(|s| s.outstanding.is_none()));
}
