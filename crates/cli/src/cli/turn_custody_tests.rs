use super::*;
use crate::cli::turn_wire::WireVerdict;
use clank_core::turn_policy::{AttendRequest, TurnEndReport, TurnPlace};

/// A check-in falling due for an agent unbound since it was armed is
/// retired with the binding, not left due: a deadline already passed would
/// wake the monitor's loop again at once, for ever.
#[test]
fn unbinding_an_attended_agent_removes_its_due_timer() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    attend_name(&mut turns, "claude", "session-one", &g);
    let due = turns.next_deadline().expect("armed");
    crate::agent_store::release_binding(repo.path(), &author, &sid("session-one")).unwrap();
    let unbound = gathered(repo.path(), "claude", &[]).at(due);
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

fn gathered(repo: &Path, who: &str, owed: &[clank_core::wait::WaitItem]) -> Gathered {
    Gathered::of(repo, &label(who), Some((owed, None)))
}

fn stop(tool: Tool, session: &str, position: Option<u64>) -> Report {
    Report::TurnEnd {
        stop: TurnEndReport {
            tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
            tool,
            session: sid(session),
            place: TurnPlace {
                turn: position.map(|p| format!("turn-{p}")),
                position,
                human_turn: false,
            },
        },
        deadline: u64::MAX,
        questions: Vec::new(),
    }
}

fn attend_name(turns: &mut Turns, who: &str, session: &str, g: &Gathered) -> String {
    turns
        .attend(
            &label(who),
            AttendRequest::Declare {
                session: sid(session),
                name: SUBAGENT.into(),
                desc: None,
                expect: 10 * MINUTE,
            },
            g,
        )
        .unwrap()
}

fn silent(why: SilentReason) -> WireVerdict {
    WireVerdict::Silent { why }
}

/// A declaration is in force the moment it is made, for every tool: the
/// whole path through the monitor's custody, persisted beside the agent
/// and shown in its projection. Nothing waits for a Stop to prove it.
#[test]
fn a_declaration_is_in_force_at_once_and_shown() {
    for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
        let repo = repo_with("agent", tool, "session-one");
        let mut turns = Turns::new(repo.path().to_path_buf());
        let g = gathered(repo.path(), "agent", &[]);
        let said = attend_name(&mut turns, "agent", "session-one", &g);
        let ends = match tool {
            Tool::Claude => "until your harness stops listing the task with that exact description",
            _ => "until you clear it with `clank attend --clear` once your harness reports it",
        };
        assert!(said.contains(ends), "{tool:?}: {said}");
        assert!(!said.contains("next turn"), "{said}");
        assert!(said.contains("after 10m"), "{said}");
        assert!(!said.contains("listed"), "{said}");
        let shown = turns.projection(&label("agent"), &g).attended;
        let [shown] = shown.as_slice() else {
            panic!("one entry: {shown:?}");
        };
        assert!(shown.declared, "{shown:?}");
        assert_eq!(shown.unqualified, None, "{tool:?}: it covers its work now");
        assert_eq!(shown.task, SUBAGENT);
        assert_eq!(
            turns.recovery_quiet(&label("agent"), &g),
            Some(Quiet::Attending),
            "{tool:?}"
        );
        assert!(
            crate::agent_store::agents_root(repo.path())
                .join("agent/turn.json")
                .exists(),
            "the monitor keeps it beside the agent"
        );
    }
}

/// A declaration Claude's listing never matches is reported once, in the
/// text of the wake that Stop already carries.
#[test]
fn a_declaration_notice_rides_the_stops_presentation() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let listed = || match stop(Tool::Claude, "session-one", None) {
        Report::TurnEnd {
            stop,
            deadline,
            questions,
        } => Report::TurnEnd {
            stop: TurnEndReport {
                tasks: clank_core::turn_policy::TaskSnapshot::Known(vec![
                    clank_core::BackgroundTask::from(serde_json::json!({"id": "a1",
                        "type": "subagent", "status": "running", "description": "sleeper"})),
                ]),
                ..stop
            },
            deadline,
            questions,
        },
        other => other,
    };
    let quiet = gathered(repo.path(), "claude", &[]);
    attend_name(&mut turns, "claude", "session-one", &quiet);
    let frame = turns.answer(&author, 1, &listed(), true, &quiet).unwrap();
    assert_eq!(frame.verdict, WireVerdict::Park, "one listing may miss");
    let owed = [message("7")];
    let g = gathered(repo.path(), "claude", &owed);
    let frame = turns.answer(&author, 2, &listed(), true, &g).unwrap();
    assert!(frame.presentation().is_some(), "{frame:?}");
    let text = frame.rendered.expect("rendered");
    assert!(text.contains("message: 7"), "{text}");
    assert!(text.contains("matched no background task"), "{text}");
    assert!(text.contains("subagent \"sleeper\" (a1)"), "{text}");
    assert!(turns.projection(&author, &g).attended.is_empty());
}

/// A restarted monitor keeps the declaration in force with its original
/// deadline.
#[test]
fn a_restart_keeps_the_declaration_and_its_deadline() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    attend_name(&mut turns, "claude", "session-one", &g);
    let due = turns.next_deadline().expect("a check-in is owed");

    let mut back = Turns::new(repo.path().to_path_buf());
    assert_eq!(back.recovery_quiet(&author, &g), Some(Quiet::Attending));
    assert_eq!(back.next_deadline(), Some(due));
}

/// With nothing attended a Stop is the owed-work decision alone, for
/// every tool: no notice about background work, because none is read.
#[test]
fn a_stop_with_nothing_attended_carries_no_notice() {
    for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
        let repo = repo_with("agent", tool, "session-one");
        let mut turns = Turns::new(repo.path().to_path_buf());
        let g = gathered(repo.path(), "agent", &[]);
        let frame = turns
            .answer(
                &label("agent"),
                1,
                &stop(tool, "session-one", None),
                true,
                &g,
            )
            .unwrap();
        assert_eq!(frame.verdict, WireVerdict::Park, "{tool:?}");
        assert_eq!(frame.rendered, None, "{tool:?}");
    }
}

/// A `clank run`'s registration: this test's own process, which is alive,
/// or the same pid under another birth, which is a reused pid and so a
/// gone run.
fn run_request(
    session: &str,
    desc: &str,
    proof: clank_core::turn_policy::ShellProof,
) -> AttendRequest {
    AttendRequest::Run {
        session: sid(session),
        proof,
        desc: desc.into(),
        expect: 10 * MINUTE,
    }
}

fn this_process() -> clank_core::turn_policy::ShellProof {
    let pid = std::process::id() as i32;
    shell_proof(
        pid,
        &crate::proc_identity::token_for(pid).expect("our own token"),
    )
}

fn reborn() -> clank_core::turn_policy::ShellProof {
    let pid = std::process::id() as i32;
    shell_proof(
        pid,
        &crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 1 },
    )
}

fn run_ends(repo: &Path, who: &str) -> Vec<(String, u64)> {
    crate::inbox::pending(repo, &label(who))
        .into_iter()
        .filter_map(|item| match item {
            clank_core::wait::WaitItem::RunEnded {
                desc,
                presentation: Some(p),
                ..
            } => Some((desc, p.seq)),
            _ => None,
        })
        .collect()
}

/// A run in flight keeps the agent attending; once its registered process
/// is gone (a reused pid here) its end is written once to the agent's
/// message log and wakes it until acked, and no listing is involved.
#[test]
fn a_gone_run_is_recorded_once_and_wakes_until_acked() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    turns
        .attend(
            &author,
            run_request("session-one", "live run", this_process()),
            &g,
        )
        .unwrap();
    turns
        .attend(
            &author,
            run_request("session-one", "test run", reborn()),
            &g,
        )
        .unwrap();
    turns.end_gone_runs(&author, &g);
    assert_eq!(run_ends(repo.path(), "claude").len(), 1);
    let [(desc, seq)] = &run_ends(repo.path(), "claude")[..] else {
        panic!("one run ended");
    };
    let seq = *seq;
    assert_eq!(desc, "test run");
    let shown = turns.projection(&author, &g).attended;
    assert_eq!(
        shown.iter().map(|a| a.task.as_str()).collect::<Vec<_>>(),
        ["live run"],
        "the live run still attends"
    );

    turns.end_gone_runs(&author, &g);
    assert_eq!(run_ends(repo.path(), "claude").len(), 1, "never twice");

    let rows = crate::cli::github::log::EventLog::open(
        &crate::agent_store::agents_root(repo.path()).join("claude/events"),
        crate::inbox::MESSAGES_SOURCE,
    )
    .unwrap()
    .read_rows()
    .unwrap()
    .unwrap()
    .rows;
    let row = rows.iter().find(|r| r.seq == seq).unwrap();
    let mut log = crate::cli::github::log::EventLog::open(
        &crate::agent_store::agents_root(repo.path()).join("claude/events"),
        crate::inbox::MESSAGES_SOURCE,
    )
    .unwrap();
    log.append_checked_ack(seq, &row.ident).unwrap();
    assert!(
        run_ends(repo.path(), "claude").is_empty(),
        "acked, it is done"
    );
    turns.end_gone_runs(&author, &g);
    assert!(
        run_ends(repo.path(), "claude").is_empty(),
        "and not re-recorded"
    );
}

/// A monitor that restarts after a run ended, with the run still in its
/// saved state, records it once; another restart records nothing more. A
/// crash between the record and forgetting the run is the same case.
#[test]
fn a_restart_records_a_run_that_ended_unwatched_exactly_once() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let g = gathered(repo.path(), "claude", &[]);
    Turns::new(repo.path().to_path_buf())
        .attend(
            &author,
            run_request("session-one", "test run", reborn()),
            &g,
        )
        .unwrap();
    let saved = crate::agent_store::agents_root(repo.path()).join("claude/turn.json");
    let before = std::fs::read(&saved).unwrap();

    Turns::new(repo.path().to_path_buf()).end_gone_runs(&author, &g);
    assert_eq!(run_ends(repo.path(), "claude").len(), 1);
    std::fs::write(&saved, &before).unwrap();
    Turns::new(repo.path().to_path_buf()).end_gone_runs(&author, &g);
    assert_eq!(
        run_ends(repo.path(), "claude").len(),
        1,
        "the run was still saved, and its record is not written twice"
    );
    Turns::new(repo.path().to_path_buf()).end_gone_runs(&author, &g);
    assert_eq!(run_ends(repo.path(), "claude").len(), 1);
}

/// Killing the command ends the run like any other end: one record.
#[test]
fn a_killed_run_is_recorded_once() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let token = crate::proc_identity::token_for(pid).expect("the child's token");
    turns
        .attend(
            &author,
            run_request("session-one", "server", shell_proof(pid, &token)),
            &g,
        )
        .unwrap();
    turns.end_gone_runs(&author, &g);
    assert!(run_ends(repo.path(), "claude").is_empty(), "still running");
    child.kill().unwrap();
    child.wait().unwrap();
    turns.end_gone_runs(&author, &g);
    turns.end_gone_runs(&author, &g);
    assert_eq!(
        run_ends(repo.path(), "claude")
            .into_iter()
            .map(|(d, _)| d)
            .collect::<Vec<_>>(),
        ["server"]
    );
}

/// The drive gate as the monitor holds it: auto off is a silence, an
/// unresolvable role is a stated failure that arms nothing, and auto off
/// outranks it.
#[test]
fn the_drive_gate_answers_before_anything_is_presented() {
    let repo = repo_with("codex", Tool::Codex, "sess-gate");
    let author = label("codex");
    let report = stop(Tool::Codex, "sess-gate", None);
    let mut turns = Turns::new(repo.path().to_path_buf());
    let owed = [message("1")];

    crate::agent_store::set_auto_mode(repo.path(), &author, AutoMode::Off).unwrap();
    let frame = turns
        .answer(
            &author,
            1,
            &report,
            true,
            &gathered(repo.path(), "codex", &owed),
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
            &gathered(repo.path(), "codex", &owed),
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
            &gathered(repo.path(), "codex", &owed),
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
/// role, with nothing added about background work.
#[test]
fn owed_work_is_presented_with_the_text_the_hook_frames() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let owed = [message("7")];
    let g = gathered(repo.path(), "claude", &owed);
    let frame = turns
        .answer(
            &author,
            1,
            &stop(Tool::Claude, "session-one", None),
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
    assert!(!text.contains("clank attend"), "{text}");

    let parked = turns
        .answer(
            &author,
            2,
            &stop(Tool::Claude, "session-one", None),
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
    let g = gathered(repo.path(), "codex", &owed);
    let mut conn = 0;
    let mut deliver = |turns: &mut Turns, position: u64, ok: bool| {
        conn += 1;
        let frame = turns
            .answer(
                &author,
                conn,
                &stop(Tool::Codex, "sess-repeat", Some(position)),
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
    let repeated = turns.projection(&author, &g).repeated;
    assert_eq!(repeated, Some(4));
}

/// The projection shows a count only once it escalates.
#[test]
fn a_count_below_the_threshold_is_not_shown() {
    let repo = repo_with("codex", Tool::Codex, "sess-below");
    let author = label("codex");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "codex", &[message("1")]);
    let frame = turns
        .answer(&author, 1, &stop(Tool::Codex, "sess-below", None), true, &g)
        .unwrap();
    let presentation = frame.presentation().unwrap().clone();
    turns
        .result(&author, 1, TurnResult::Delivered { presentation }, &g)
        .unwrap();
    assert_eq!(turns.projection(&author, &g).repeated, None);
}

/// Only the connection that carried a presentation can answer for it.
/// A result from any other connection — an older hook's, a client's —
/// commits nothing, and the carrier's own result still commits once.
#[test]
fn a_result_from_another_connection_is_not_credited() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[message("1")]);
    let frame = turns
        .answer(
            &author,
            10,
            &stop(Tool::Claude, "session-one", None),
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
    let g = gathered(repo.path(), "claude", &[message("1")]);
    let report = stop(Tool::Claude, "session-one", None);
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

/// An agent no recovery has identified, so with no typed route, is
/// quieted by its first qualifying Stop, which arms the check-in; one that
/// cannot be typed is recorded undeliverable, which is shown, and recovery
/// treats the agent as any other past the deadline.
#[test]
fn a_silence_needs_no_route_and_a_lost_check_in_is_recorded() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    attend_name(&mut turns, "claude", "session-one", &g);
    let report = stop(Tool::Claude, "session-one", None);
    let frame = turns.answer(&author, 1, &report, true, &g).unwrap();
    assert_eq!(frame.verdict, WireVerdict::Park);
    let due = turns.next_deadline().expect("armed by the first Stop");
    let at_due = gathered(repo.path(), "claude", &[]).at(due);
    let check_in = turns.check_in(&author, &at_due).expect("owed");
    assert_eq!(check_in.session, sid("session-one"));
    let text = turns
        .may_type_check_in(&author, &check_in.check_ins, &at_due)
        .expect("the last look lets it through");
    assert!(text.contains("check-in"), "{text}");
    assert!(
        text.contains(&format!("clank attend '{SUBAGENT}' --expect 20m")),
        "{text}"
    );
    turns.check_in_failed(
        &author,
        check_in.check_ins.clone(),
        "no pane was found".into(),
        &at_due,
    );
    let shown = turns.projection(&author, &at_due).attended;
    assert_eq!(
        shown.first().and_then(|a| a.check_in.as_deref()),
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
    let Report::TurnEnd { stop, deadline, .. } = stop(Tool::Claude, "session-one", None) else {
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
    );
    let report = match tool {
        Tool::Claude => stop(tool, &fixture.session, None),
        _ => stop(tool, &fixture.session, None),
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

/// With no monitor, an attendance quiets nothing: the notice still wakes
/// the session clank drives, and only that session.
#[test]
fn a_no_monitor_notice_ignores_attendance_and_honours_the_drive_gate() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    attend_name(&mut turns, "claude", "session-one", &g);
    turns
        .answer(
            &author,
            1,
            &stop(Tool::Claude, "session-one", None),
            true,
            &g,
        )
        .unwrap();
    let notice = || clank_core::turn_policy::Notice::MonitorOutage {
        why: "no binary".into(),
    };
    let unmonitored = Gathered::of(repo.path(), &author, None);
    assert_eq!(
        unmonitored.unmonitored_verdict(&sid("session-one"), notice()),
        Verdict::Notify(vec![notice()])
    );
    assert_eq!(
        unmonitored.unmonitored_verdict(&sid("someone-else-entirely"), notice()),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    crate::agent_store::set_auto_mode(repo.path(), &author, AutoMode::Off).unwrap();
    assert_eq!(
        Gathered::of(repo.path(), &author, None).unmonitored_verdict(&sid("session-one"), notice()),
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
    let report = stop(Tool::Codex, "sess-role", None);
    let parked = turns
        .answer(
            &author,
            1,
            &report,
            true,
            &gathered(repo.path(), "codex", &[]),
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
            &gathered(repo.path(), "codex", &[message("1")]),
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
    let unfolded = Gathered::of(repo.path(), &author, None);
    let handoff = Report::Handoff {
        tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
        session: sid("session-one"),
        continued: false,
        deadline: u64::MAX,
    };
    let frame = turns
        .answer(&author, 1, &handoff, true, &unfolded)
        .expect("answered unfolded");
    assert_eq!(frame.verdict, WireVerdict::Notify);
    assert_eq!(
        frame.rendered.as_deref(),
        Some(super::super::wake_render::ARM_WAIT)
    );
    let frame = turns
        .answer(
            &author,
            2,
            &handoff,
            true,
            &Gathered::of(repo.path(), &author, None).waiting(true),
        )
        .unwrap();
    assert_eq!(frame.verdict, silent(SilentReason::WaitArmed));
    assert!(turns.state(&author).is_none_or(|s| s.outstanding.is_none()));

    attend_name(&mut turns, "grok", "session-one", &unfolded);
    for conn in 3..6 {
        turns
            .answer(&author, conn, &handoff, true, &unfolded)
            .unwrap();
    }
    assert_eq!(
        turns.projection(&author, &unfolded).attended.len(),
        1,
        "Stops without a listing end no declaration"
    );
    let listed = |tasks: serde_json::Value| Report::Handoff {
        tasks: clank_core::turn_policy::TaskSnapshot::Known(serde_json::from_value(tasks).unwrap()),
        session: sid("session-one"),
        continued: false,
        deadline: u64::MAX,
    };
    let running = listed(serde_json::json!([{"id": "g1", "type": "subagent",
        "status": "running", "description": SUBAGENT, "agentType": "explore"}]));
    let frame = turns.answer(&author, 6, &running, true, &unfolded).unwrap();
    assert_eq!(
        frame.rendered.as_deref(),
        Some(super::super::wake_render::ARM_WAIT)
    );
    assert_eq!(
        turns.state(&author).unwrap().attendances[0].task(),
        Some("g1"),
        "resolved by Grok's own listing, and persisted"
    );
    turns
        .answer(&author, 7, &running, false, &unfolded)
        .unwrap();
    turns
        .answer(&author, 8, &listed(serde_json::json!([])), false, &unfolded)
        .unwrap();
    assert_eq!(
        turns.projection(&author, &unfolded).attended.len(),
        1,
        "a re-step is no Stop"
    );
    turns
        .answer(&author, 9, &listed(serde_json::json!([])), true, &unfolded)
        .unwrap();
    assert!(
        turns.projection(&author, &unfolded).attended.is_empty(),
        "Grok stopped listing it: the declaration is over"
    );
}

fn ask_and_answer(repo: &Path, who: &str, name: &str, question: &str, answer: &str) {
    crate::cli::block::ask(repo, &label(who), name, question).unwrap();
    crate::cli::block::answer_block(repo, who, name, answer, None).unwrap();
}

/// What the monitor would owe `who` from its blocks right now.
fn owed_blocks(repo: &Path, who: &str) -> Vec<clank_core::wait::WaitItem> {
    crate::cli::wait::block_items(repo, &label(who))
}

fn receipt_marker(repo: &Path, who: &str, name: &str) -> PathBuf {
    crate::agent_store::agents_root(repo).join(format!("{who}/unblocks/{name}.received"))
}

fn idle_stop() -> Report {
    stop(Tool::Claude, "session-one", None)
}

/// Present what is owed now on connection `conn`, and return the frame
/// with the commit its `Delivered` makes.
fn present_owed(turns: &mut Turns, repo: &Path, conn: u64) -> (Frame, Option<Commit>) {
    let author = label("claude");
    let g = gathered(repo, "claude", &owed_blocks(repo, "claude"));
    let frame = turns.answer(&author, conn, &idle_stop(), true, &g).unwrap();
    let presentation = frame.presentation().expect("presented").clone();
    let commit = turns
        .result(&author, conn, TurnResult::Delivered { presentation }, &g)
        .unwrap();
    (frame, commit)
}

/// A delivered answer is received: its marker names that exact pair and
/// the next turn end carries no `unblocked` item.
#[tokio::test]
async fn a_delivered_answer_is_received_and_not_presented_again() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    ask_and_answer(repo.path(), "claude", "q", "Proceed?", "yes");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let (frame, commit) = present_owed(&mut turns, repo.path(), 1);
    let text = frame.rendered.expect("rendered");
    assert!(text.contains("unblocked: q: yes"), "{text}");
    assert!(!text.contains("block clean"), "{text}");
    commit.expect("a delivery commits").run(repo.path()).await;
    assert!(receipt_marker(repo.path(), "claude", "q").exists());
    assert!(owed_blocks(repo.path(), "claude").is_empty());

    let mut restarted = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &owed_blocks(repo.path(), "claude"));
    let frame = restarted
        .answer(&label("claude"), 2, &idle_stop(), true, &g)
        .unwrap();
    assert!(
        frame
            .rendered
            .as_deref()
            .is_none_or(|t| !t.contains("unblocked")),
        "a restarted monitor does not present a received answer: {frame:?}"
    );
}

/// A presentation that never landed receives nothing: the answer is
/// presented again.
#[tokio::test]
async fn an_answer_whose_presentation_failed_stays_owed() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    ask_and_answer(repo.path(), "claude", "q", "Proceed?", "yes");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &owed_blocks(repo.path(), "claude"));
    let failed = turns.answer(&author, 1, &idle_stop(), true, &g).unwrap();
    let presentation = failed.presentation().unwrap().clone();
    let emit_failed = TurnResult::EmitFailed {
        presentation,
        why: "broken pipe".into(),
    };
    assert!(turns.result(&author, 1, emit_failed, &g).unwrap().is_none());
    let unanswered = turns.answer(&author, 2, &idle_stop(), true, &g).unwrap();
    assert!(unanswered.presentation().is_some());
    turns.closed(&author, 2, &g);
    assert!(!receipt_marker(repo.path(), "claude", "q").exists());
    let again = owed_blocks(repo.path(), "claude");
    assert!(
        matches!(
            again.as_slice(),
            [clank_core::wait::WaitItem::Unblocked { .. }]
        ),
        "{again:?}"
    );
    let (frame, _) = present_owed(&mut turns, repo.path(), 3);
    assert!(frame.rendered.unwrap().contains("unblocked: q: yes"));
}

/// The binding moved on between delivery and commit: nothing is received.
#[tokio::test]
async fn an_answer_delivered_to_a_replaced_session_stays_owed() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    ask_and_answer(repo.path(), "claude", "q", "Proceed?", "yes");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let (_, commit) = present_owed(&mut turns, repo.path(), 1);
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &label("claude"),
        Tool::Claude,
        &sid("session-two"),
    )
    .unwrap();
    commit.unwrap().run(repo.path()).await;
    assert!(!receipt_marker(repo.path(), "claude", "q").exists());
    assert_eq!(owed_blocks(repo.path(), "claude").len(), 1);
}

/// Two answered blocks, one presented: only that one is received.
#[tokio::test]
async fn only_the_presented_answer_is_received() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    ask_and_answer(repo.path(), "claude", "first", "One?", "yes");
    ask_and_answer(repo.path(), "claude", "second", "Two?", "no");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let (frame, commit) = present_owed(&mut turns, repo.path(), 1);
    let text = frame.rendered.unwrap();
    assert!(
        text.contains("unblocked: first: yes") && !text.contains("second"),
        "{text}"
    );
    commit.unwrap().run(repo.path()).await;
    assert!(receipt_marker(repo.path(), "claude", "first").exists());
    assert!(!receipt_marker(repo.path(), "claude", "second").exists());
    let (frame, _) = present_owed(&mut turns, repo.path(), 2);
    assert!(frame.rendered.unwrap().contains("unblocked: second: no"));
}

/// Present answer A, then ask the same question again and give it the
/// same answer. A's late receipt is a no-op whether it lands at once,
/// after a clean, or after the monitor restarted: the new occurrence is
/// a different item and stays owed.
#[tokio::test]
async fn a_late_receipt_after_identical_reuse_receives_nothing() {
    for interleave in ["direct", "clean", "restart"] {
        let repo = repo_with("claude", Tool::Claude, "session-one");
        ask_and_answer(repo.path(), "claude", "q", "Proceed?", "yes");
        let mut turns = Turns::new(repo.path().to_path_buf());
        let first_owed = owed_blocks(repo.path(), "claude");
        let (_, commit) = present_owed(&mut turns, repo.path(), 1);
        let late = commit.expect("delivered");

        ask_and_answer(repo.path(), "claude", "q", "Proceed?", "yes");
        match interleave {
            "clean" => {
                crate::cli::block::clean(repo.path(), &label("claude")).unwrap();
            }
            "restart" => turns = Turns::new(repo.path().to_path_buf()),
            _ => {}
        }
        let second_owed = owed_blocks(repo.path(), "claude");
        let identity = |items: &[clank_core::wait::WaitItem]| {
            super::super::wake_render::mirror(items, None)[0].identity()
        };
        assert_ne!(
            identity(&first_owed),
            identity(&second_owed),
            "{interleave}: identical text, a different occurrence"
        );

        late.run(repo.path()).await;
        assert!(
            !receipt_marker(repo.path(), "claude", "q").exists(),
            "{interleave}"
        );
        assert_eq!(
            owed_blocks(repo.path(), "claude"),
            second_owed,
            "{interleave}: the new occurrence stays owed"
        );
        let (frame, _) = present_owed(&mut turns, repo.path(), 2);
        assert!(
            frame.rendered.unwrap().contains("unblocked: q: yes"),
            "{interleave}"
        );
    }
}

/// While attending, a delivery renders only the work this session has not
/// received, and what it received survives a monitor restart in turn.json.
#[test]
fn an_attending_delivery_renders_only_unreceived_work_across_a_restart() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let report = stop(Tool::Claude, "session-one", None);
    let mut turns = Turns::new(repo.path().to_path_buf());
    let one = gathered(repo.path(), "claude", &[message("1")]);
    let frame = turns.answer(&author, 1, &report, true, &one).unwrap();
    let presentation = frame.presentation().expect("never received").clone();
    turns
        .result(&author, 1, TurnResult::Delivered { presentation }, &one)
        .unwrap();
    turns
        .attend(
            &author,
            run_request("session-one", "test run", this_process()),
            &one,
        )
        .unwrap();

    let mut back = Turns::new(repo.path().to_path_buf());
    let frame = back.answer(&author, 2, &report, true, &one).unwrap();
    assert_eq!(frame.verdict, WireVerdict::Park);
    let both = gathered(repo.path(), "claude", &[message("1"), message("2")]);
    let frame = back.answer(&author, 3, &report, true, &both).unwrap();
    let text = frame.rendered.unwrap_or_default();
    assert!(
        text.contains("messages@2") && !text.contains("messages@1"),
        "{text}"
    );
}

/// The monitor's set of agents a user command waits on reaches the model:
/// a parked Claude report stepped again ends silently with the reason, and
/// clearing the set parks it again.
#[test]
fn a_parked_report_ends_while_a_user_command_waits_for_its_agent() {
    let repo = repo_with("claude", Tool::Claude, "session-uc");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    let report = stop(Tool::Claude, "session-uc", None);
    let first = turns.answer(&author, 1, &report, true, &g).unwrap();
    assert_eq!(first.verdict, WireVerdict::Park);
    turns.set_user_commands([author.clone()].into_iter().collect());
    let released = turns.answer(&author, 1, &report, false, &g).unwrap();
    assert_eq!(released.verdict, silent(SilentReason::UserCommand));
    turns.set_user_commands(Default::default());
    let parked = turns.answer(&author, 1, &report, false, &g).unwrap();
    assert_eq!(parked.verdict, WireVerdict::Park);
}

/// Only the kernel's word that the process ended, or another birth under
/// its pid, is gone; a denied, failed or unrecognisable read is unknown.
#[test]
fn only_an_explicit_end_or_another_birth_is_a_gone_process() {
    use crate::proc_identity::ProcToken;
    use crate::proc_identity::inspect::{Inspection, ProcessDetails, ProcessState, Unavailable};
    let token = ProcToken::MacStart { sec: 5, usec: 6 };
    let proof = shell_proof(4242, &token);
    let details = |state| {
        Inspection::Verified(ProcessDetails {
            pid: 4242,
            birth: token.clone(),
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
            argv: Ok(Vec::new()),
            state,
            parent: Ok(1),
            group: Ok(1),
        })
    };
    let cases = [
        (details(Ok(ProcessState::Sleeping)), Presence::Live),
        (
            details(Err(Unavailable::Io(std::io::ErrorKind::PermissionDenied))),
            Presence::Live,
        ),
        (details(Ok(ProcessState::Zombie)), Presence::Gone),
        (Inspection::Ended, Presence::Gone),
        (Inspection::Mismatched, Presence::Gone),
        (
            Inspection::Unavailable(Unavailable::Io(std::io::ErrorKind::PermissionDenied)),
            Presence::Unknown,
        ),
        (
            Inspection::Unavailable(Unavailable::Io(std::io::ErrorKind::Interrupted)),
            Presence::Unknown,
        ),
        (
            Inspection::Unavailable(Unavailable::Unsupported),
            Presence::Unknown,
        ),
        (
            Inspection::Unavailable(Unavailable::Malformed),
            Presence::Unknown,
        ),
    ];
    for (seen, want) in cases {
        let label = format!("{seen:?}");
        assert_eq!(presence_of(&proof, |_, _| seen), want, "{label}");
    }
    for token in ["not json", r#"{"v":"future9","x":1}"#] {
        let unreadable = clank_core::turn_policy::ShellProof {
            pid: 4242,
            token: token.into(),
        };
        let seen = presence_of(&unreadable, |pid, t| {
            if matches!(t, ProcToken::Unrecognised) {
                Inspection::Unavailable(Unavailable::Unsupported)
            } else {
                panic!("{pid}: a malformed token is never inspected")
            }
        });
        assert_eq!(seen, Presence::Unknown, "{token}");
    }
}

/// A registered run whose identity cannot be read is kept and records no
/// end; the same run under a confirmed other birth records one.
#[test]
fn an_unreadable_run_is_kept_and_records_no_end() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    let unreadable = clank_core::turn_policy::ShellProof {
        pid: std::process::id() as i32,
        token: r#"{"v":"future9"}"#.into(),
    };
    turns
        .attend(
            &author,
            run_request("session-one", "odd run", unreadable),
            &g,
        )
        .unwrap();
    turns
        .attend(
            &author,
            run_request("session-one", "test run", reborn()),
            &g,
        )
        .unwrap();
    turns.end_gone_runs(&author, &g);
    turns.end_gone_runs(&author, &g);
    assert_eq!(
        run_ends(repo.path(), "claude")
            .into_iter()
            .map(|(d, _)| d)
            .collect::<Vec<_>>(),
        ["test run"]
    );
    let shown = turns.projection(&author, &g).attended;
    assert_eq!(
        shown.iter().map(|a| a.task.as_str()).collect::<Vec<_>>(),
        ["odd run"]
    );
    assert_eq!(
        shown[0].unqualified.as_deref(),
        Some("process cannot be seen")
    );
}

/// A registration that cannot be written is refused and leaves nothing
/// behind: the monitor never acknowledges a run a restart would lose, so
/// `clank run` never execs it.
#[test]
fn a_registration_that_cannot_be_written_is_refused() {
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "claude", &[]);
    let stored = crate::agent_store::agents_root(repo.path()).join("claude/turn.json");
    std::fs::create_dir_all(&stored).unwrap();
    let refused = turns
        .attend(
            &author,
            run_request("session-one", "test run", this_process()),
            &g,
        )
        .expect_err("not durable, not accepted");
    assert!(refused.contains("nothing was recorded"), "{refused}");
    assert!(turns.projection(&author, &g).attended.is_empty());
    assert_eq!(turns.next_deadline(), None);
    turns.end_gone_runs(&author, &g);
    assert!(run_ends(repo.path(), "claude").is_empty());

    std::fs::remove_dir(&stored).unwrap();
    turns
        .attend(
            &author,
            run_request("session-one", "test run", this_process()),
            &g,
        )
        .expect("once it can be written, it is accepted");
    assert!(stored.is_file());
}

/// A crash between writing a run's end and retiring its registration
/// restores a registration that replays the record; the record is never
/// written twice however far the log's identity horizon has moved on and
/// however it was compacted and acked meanwhile.
#[test]
fn a_replayed_end_is_not_written_again_past_the_identity_horizon() {
    use crate::cli::github::log::{COMPACT_AT, EventLog, LOG_IDENTITY_CAP, Transport};
    let repo = repo_with("claude", Tool::Claude, "session-one");
    let author = label("claude");
    let g = gathered(repo.path(), "claude", &[]);
    Turns::new(repo.path().to_path_buf())
        .attend(
            &author,
            run_request("session-one", "test run", reborn()),
            &g,
        )
        .unwrap();
    let saved = crate::agent_store::agents_root(repo.path()).join("claude/turn.json");
    let stale = std::fs::read(&saved).unwrap();
    Turns::new(repo.path().to_path_buf()).end_gone_runs(&author, &g);
    let [(_, seq)] = run_ends(repo.path(), "claude")[..] else {
        panic!("one end");
    };

    let events = crate::agent_store::agents_root(repo.path()).join("claude/events");
    let mut log = EventLog::open(&events, crate::inbox::MESSAGES_SOURCE).unwrap();
    log.load();
    let ident = log
        .read_rows()
        .unwrap()
        .unwrap()
        .rows
        .into_iter()
        .find(|r| r.seq == seq)
        .unwrap()
        .ident;
    log.append_checked_ack(seq, &ident).unwrap();
    for i in 0..COMPACT_AT {
        let item = message(&format!("filler-{i}"));
        log.append_inbox(
            Transport::Message,
            None,
            Some(&format!("message#claude/filler-{i}")),
            None,
            &item,
        )
        .unwrap();
    }
    assert!(log.maybe_compact().is_ok());
    let mut log = EventLog::open(&events, crate::inbox::MESSAGES_SOURCE).unwrap();
    log.load();
    for i in 0..=LOG_IDENTITY_CAP {
        log.append_inbox(
            Transport::Message,
            None,
            Some(&format!("message#claude/later-{i}")),
            None,
            &message(&format!("later-{i}")),
        )
        .unwrap();
    }
    let horizon = EventLog::open(&events, crate::inbox::MESSAGES_SOURCE)
        .unwrap()
        .load()
        .action_keys;
    assert!(
        !horizon.contains(&"message#claude/filler-0".to_string()),
        "the horizon moved past older keys"
    );
    assert!(
        horizon.iter().any(|k| k.starts_with("run-ended#")),
        "and kept the pinned end: {} keys, {:?}",
        horizon.len(),
        horizon
            .iter()
            .filter(|k| !k.starts_with("message#"))
            .collect::<Vec<_>>()
    );
    assert!(horizon.len() <= LOG_IDENTITY_CAP + 1);

    std::fs::write(&saved, &stale).unwrap();
    Turns::new(repo.path().to_path_buf()).end_gone_runs(&author, &g);
    assert!(
        run_ends(repo.path(), "claude").is_empty(),
        "the replay wrote no second end"
    );
    assert!(
        Turns::new(repo.path().to_path_buf())
            .projection(&author, &g)
            .attended
            .is_empty(),
        "and the stale registration is retired"
    );
}

fn presented(turns: &mut Turns, conn: u64, g: &Gathered) -> PresentationId {
    let frame = turns
        .answer(
            &label("agent"),
            conn,
            &stop(Tool::Codex, "session-one", Some(conn)),
            true,
            g,
        )
        .unwrap();
    frame
        .presentation()
        .unwrap_or_else(|| panic!("owed work is presented: {frame:?}"))
        .clone()
}

fn evidence(turns: &Turns, repo: &Path, session: &str) -> crate::cli::ledger::Evidence {
    crate::cli::ledger::Evidence {
        delivered: crate::cli::presentation::delivered_to(
            &crate::agent_store::agents_root(repo).join("agent"),
            session,
        ),
        in_flight: turns.in_flight(&label("agent")),
    }
}

/// A live carrier whose presentation never settles is a delivery in
/// progress only until [`clank_core::turn_policy::DELIVERY_RESULT_WITHIN`]
/// has passed. Then custody fails it: nothing is in flight, the item is
/// not delivered, the carrier's late success is refused and commits
/// nothing, and the next Stop is presented the work afresh rather than
/// parked behind a presentation that will never end.
#[tokio::test]
async fn a_carrier_that_never_answers_is_given_up_at_the_result_bound() {
    use crate::cli::ledger::{Delivery, delivery};
    use clank_core::turn_policy::DELIVERY_RESULT_WITHIN;
    const T0: Ms = 1_000_000;
    let repo = repo_with("agent", Tool::Codex, "session-one");
    let author = label("agent");
    let owed = [message("m1")];
    let identity = crate::cli::wake_render::mirror(&owed, None)[0].identity();
    let mut turns = Turns::new(repo.path().to_path_buf());
    let at = |now| gathered(repo.path(), "agent", &owed).at(now);

    let stuck = presented(&mut turns, 1, &at(T0));
    let expires = T0 + DELIVERY_RESULT_WITHIN;
    assert_eq!(
        turns.next_deadline(),
        Some(expires),
        "the bound wakes the loop"
    );
    assert_eq!(
        turns.in_flight(&author),
        Some((vec![identity.clone()], expires / 1000))
    );
    let seen = |turns: &Turns, now: Ms| {
        delivery(
            &identity,
            &evidence(turns, repo.path(), "session-one"),
            now / 1000,
        )
    };
    assert_eq!(seen(&turns, T0), Delivery::InFlight(expires / 1000));
    let parked = turns
        .answer(
            &author,
            2,
            &stop(Tool::Codex, "session-one", Some(2)),
            true,
            &at(T0 + 5),
        )
        .unwrap();
    assert_eq!(
        parked.verdict,
        WireVerdict::Park,
        "nothing new while it flies"
    );

    assert!(turns.overdue_authors(expires - 1).is_empty());
    assert!(!turns.expire(&author, &at(expires - 1)), "inside its bound");
    assert!(turns.presenting(&author));

    assert_eq!(
        turns.overdue_authors(expires),
        std::slice::from_ref(&author)
    );
    assert!(turns.expire(&author, &at(expires)));
    assert!(!turns.presenting(&author));
    assert_eq!(
        turns.next_deadline(),
        None,
        "and the bound is not due again"
    );
    assert_eq!(turns.in_flight(&author), None);
    assert_eq!(seen(&turns, expires), Delivery::NotDelivered);
    assert_eq!(
        turns.carrier(1),
        None,
        "its carrier answers for nothing now"
    );

    let late = turns.result(
        &author,
        1,
        TurnResult::Delivered {
            presentation: stuck.clone(),
        },
        &at(expires + 5),
    );
    assert!(
        late.is_err(),
        "a late success is refused: {:?}",
        late.ok().map(|c| c.is_some())
    );
    assert_eq!(seen(&turns, expires + 5), Delivery::NotDelivered);

    let again = presented(&mut turns, 3, &at(expires + 10));
    assert_ne!(again, stuck, "the work is presented afresh, on a new bound");
    assert_eq!(
        turns.next_deadline(),
        Some(expires + 10 + DELIVERY_RESULT_WITHIN)
    );
}

/// Only a positive result proves a delivery. A failed emit and a closed
/// connection leave the item undelivered; a `Delivered` writes the bound
/// session's receipt before any backend callback runs, so a callback that
/// is still pending, or never comes, undoes nothing.
#[tokio::test]
async fn only_a_positive_result_establishes_a_delivery() {
    use crate::cli::ledger::{Delivery, delivery};
    let repo = repo_with("agent", Tool::Codex, "session-one");
    let author = label("agent");
    let owed = [message("m1")];
    let identity = crate::cli::wake_render::mirror(&owed, None)[0].identity();
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "agent", &owed);
    let seen = |turns: &Turns| {
        delivery(
            &identity,
            &evidence(turns, repo.path(), "session-one"),
            crate::cli::recovery::unix_now(),
        )
    };

    let failed = presented(&mut turns, 1, &g);
    let commit = turns
        .result(
            &author,
            1,
            TurnResult::EmitFailed {
                presentation: failed,
                why: "nothing reached the agent".into(),
            },
            &g,
        )
        .unwrap();
    assert!(commit.is_none(), "a failed emit commits nothing");
    assert_eq!(seen(&turns), Delivery::NotDelivered);

    presented(&mut turns, 2, &g);
    turns.closed(&author, 2, &g);
    assert_eq!(
        seen(&turns),
        Delivery::NotDelivered,
        "a result that never came"
    );

    let delivered = presented(&mut turns, 3, &g);
    let commit = turns
        .result(
            &author,
            3,
            TurnResult::Delivered {
                presentation: delivered,
            },
            &g,
        )
        .unwrap()
        .expect("a delivery commits");
    assert_eq!(
        seen(&turns),
        Delivery::NotDelivered,
        "not before it is recorded"
    );
    // The record alone, as `Commit::run` makes it before it dispatches a
    // single receipt callback.
    crate::cli::presentation::record_delivered(
        repo.path(),
        &author,
        Tool::Codex,
        "session-one",
        &crate::cli::presentation::Carried {
            events: Vec::new(),
            answers: Vec::new(),
            identities: commit.presented().to_vec(),
        },
        || true,
    );
    assert_eq!(
        seen(&turns),
        Delivery::Delivered,
        "with every callback still pending"
    );

    let other = crate::cli::ledger::Evidence {
        delivered: crate::cli::presentation::delivered_to(
            &crate::agent_store::agents_root(repo.path()).join("agent"),
            "session-two",
        ),
        in_flight: None,
    };
    assert_eq!(
        delivery(&identity, &other, 0),
        Delivery::NotDelivered,
        "and it proves nothing to any other session"
    );
}

/// A wake that reached the model through the monitor leaves the bound
/// session's receipt for every item it carried; one whose session lost
/// the label before it was recorded leaves none.
#[tokio::test]
async fn a_committed_delivery_leaves_the_bound_sessions_receipt() {
    let repo = repo_with("agent", Tool::Codex, "session-one");
    let author = label("agent");
    let owed = [message("m1")];
    let identity = crate::cli::wake_render::mirror(&owed, None)[0].identity();
    let dir = crate::agent_store::agents_root(repo.path()).join("agent");
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gathered(repo.path(), "agent", &owed);
    let deliver = |turns: &mut Turns, conn| {
        let presentation = presented(turns, conn, &g);
        turns
            .result(&author, conn, TurnResult::Delivered { presentation }, &g)
            .unwrap()
            .expect("a delivery commits")
    };

    deliver(&mut turns, 1).run(repo.path()).await;
    assert!(
        crate::cli::presentation::delivered_to(&dir, "session-one").contains_key(&identity),
        "the session that saw it holds its receipt"
    );

    std::fs::remove_file(crate::cli::presentation::receipts_path(&dir)).unwrap();
    let commit = deliver(&mut turns, 2);
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &author,
        Tool::Codex,
        &sid("session-two"),
    )
    .unwrap();
    commit.run(repo.path()).await;
    assert!(
        !crate::cli::presentation::receipts_path(&dir).exists(),
        "a session that no longer holds the label is given no receipt"
    );
}
