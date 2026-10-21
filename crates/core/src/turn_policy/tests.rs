use super::*;
use crate::hook_io::HookInput;

const RUNNING: &str = include_str!("../../tests/fixtures/stop_hook/stop_subagent_running.json");
const FINISHED: &str = include_str!("../../tests/fixtures/stop_hook/stop_subagent_finished.json");
const MONITOR: &str = include_str!("../../tests/fixtures/stop_hook/stop_monitor_task_running.json");

const SUBAGENT_ID: &str = "a25eee2eef81caf3e";
const SUBAGENT_NAME: &str = "capture-probe sleeper";
const MINUTE: Ms = 60_000;
const T0: Ms = 1_000_000;

struct Alive(bool);
impl Liveness for Alive {
    fn holds(&self, _: &ShellProof) -> bool {
        self.0
    }
}
static LIVE: Alive = Alive(true);
static DEAD: Alive = Alive(false);

fn session() -> SessionId {
    SessionId::parse("00000000-0000-0000-0000-000000000000").unwrap()
}

fn other_session() -> SessionId {
    SessionId::parse("11111111-1111-1111-1111-111111111111").unwrap()
}

fn tasks(fixture: &str) -> TaskSnapshot {
    let input: HookInput = serde_json::from_str(fixture).unwrap();
    TaskSnapshot::of(&input)
}

fn shell_task(id: &str) -> BackgroundTask {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "type": "shell",
        "status": "running",
        "command": "clank run --desc \"long build\" -- cargo build",
    }))
    .unwrap()
}

fn facts<'a>(now: Ms, binding: &'a SessionId, owed: &'a [OwedItem]) -> Facts<'a> {
    Facts {
        now,
        binding: Some(binding),
        auto: Ok(AutoMode::On),
        role: Ok(Role::Master),
        asking: false,
        blocked: false,
        owed,
        liveness: &LIVE,
        check_in_route: Ok(()),
    }
}

fn report(tool: Tool, tasks: TaskSnapshot) -> TurnEndReport {
    TurnEndReport {
        tool,
        session: session(),
        place: TurnPlace::default(),
        tasks,
    }
}

fn attend(selector: Selector, shell: Option<ShellProof>) -> Event {
    Event::Attend(AttendRequest::Attend {
        session: session(),
        selector,
        desc: Some("probe".into()),
        expect: 10 * MINUTE,
        shell,
    })
}

fn proof() -> Option<ShellProof> {
    Some(ShellProof {
        pid: 4242,
        token: "mac1:1792561803:751598".into(),
    })
}

fn verdict(out: Output) -> Verdict {
    match out {
        Output::Verdict(v) => v,
        other => panic!("not a verdict: {other:?}"),
    }
}

fn effects(out: Output) -> Vec<Effect> {
    match out {
        Output::Effects(e) => e,
        other => panic!("not effects: {other:?}"),
    }
}

fn settled(out: Output) -> ResultOutcome {
    match out {
        Output::Result(r) => r,
        other => panic!("not a result: {other:?}"),
    }
}

fn committed(out: Output) -> Vec<Effect> {
    match settled(out) {
        ResultOutcome::Resolved(e) => e,
        other => panic!("not resolved: {other:?}"),
    }
}

fn attending() -> Verdict {
    Verdict::Silent(SilentWhy::Attending {
        task_id: SUBAGENT_ID.into(),
    })
}

/// A session attending the captured subagent, proven by one Stop.
fn standing(s: &SessionId) -> SessionState {
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(T0, s, &[]),
    );
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
            &facts(T0 + MINUTE, s, &[])
        )),
        attending()
    );
    st
}

fn since(st: &SessionState) -> Ms {
    match &st.attendance {
        Attendance::Standing { since, .. } | Attendance::Intent { since, .. } => *since,
        Attendance::None => panic!("no attendance"),
    }
}

#[test]
fn the_captured_payloads_parse_with_every_field_kept() {
    let running: HookInput = serde_json::from_str(RUNNING).unwrap();
    let [task] = running.background_tasks.as_deref().unwrap() else {
        panic!("one task");
    };
    assert_eq!(task.id.as_deref(), Some(SUBAGENT_ID));
    assert_eq!(TaskKind::of(task), TaskKind::Subagent);
    assert_eq!(task.description.as_deref(), Some(SUBAGENT_NAME));
    assert_eq!(task.agent_type.as_deref(), Some("general-purpose"));
    assert_eq!(task.raw["agent_type"], "general-purpose");

    let finished: HookInput = serde_json::from_str(FINISHED).unwrap();
    assert_eq!(finished.background_tasks, Some(Vec::new()));

    let monitor: HookInput = serde_json::from_str(MONITOR).unwrap();
    assert_eq!(
        TaskKind::of(&monitor.background_tasks.unwrap()[0]),
        TaskKind::Unknown(Some("monitor".into()))
    );
}

#[test]
fn t01_a_subagent_attended_by_name_is_silenced_by_the_stop_that_lists_it() {
    let s = session();
    let st = standing(&s);
    assert!(matches!(
        &st.attendance,
        Attendance::Standing { task_id, kind: TaskKind::Subagent, check_in: CheckIn::Armed, .. }
            if task_id == SUBAGENT_ID
    ));
}

#[test]
fn t02_an_unrelated_wake_leaves_the_attendance_standing() {
    let s = session();
    let mut st = standing(&s);
    let first = since(&st);
    let stray = PresentationId {
        session: s.clone(),
        seq: 99,
    };
    assert_eq!(
        settled(st.step(
            Event::Delivered(stray.clone()),
            &facts(T0 + 2 * MINUTE, &s, &[])
        )),
        ResultOutcome::ProtocolViolation(stray.clone())
    );
    st.step(Event::EmitFailed(stray), &facts(T0 + 2 * MINUTE, &s, &[]));
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
            &facts(T0 + 3 * MINUTE, &s, &[])
        )),
        attending()
    );
    assert_eq!(since(&st), first);
}

#[test]
fn t03_a_stop_without_the_task_ends_the_attendance_and_parks() {
    let s = session();
    let mut st = standing(&s);
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
            &facts(T0 + 2 * MINUTE, &s, &[])
        )),
        Verdict::Park
    );
    assert_eq!(st.attendance, Attendance::None);
}

#[test]
fn t04_an_intent_made_before_its_task_is_listed_resolves_at_the_next_stop() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0, &s, &[]),
    );
    st.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(T0, &s, &[]),
    );
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
            &facts(T0 + MINUTE, &s, &[])
        )),
        attending()
    );
}

#[test]
fn t05_no_match_or_several_wake_with_the_live_tasks_and_drop_the_intent() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::Name("nope".into()), None),
        &facts(T0, &s, &[]),
    );
    let v = verdict(st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
        &facts(T0, &s, &[]),
    ));
    let Verdict::Notify(Notice::NoMatch { live, .. }) = v else {
        panic!("{v:?}");
    };
    assert_eq!(live[0].description.as_deref(), Some(SUBAGENT_NAME));
    assert_eq!(st.attendance, Attendance::None);

    let mut twins = shell_task("one");
    twins.description = Some("same".into());
    let mut other = shell_task("two");
    other.description = Some("same".into());
    st.step(
        attend(Selector::Name("same".into()), proof()),
        &facts(T0, &s, &[]),
    );
    let v = verdict(st.step(
        Event::TurnEnd(report(
            Tool::Claude,
            TaskSnapshot::Known(vec![twins, other]),
        )),
        &facts(T0, &s, &[]),
    ));
    assert!(
        matches!(v, Verdict::Notify(Notice::Ambiguous { ref matches, .. }) if matches.len() == 2),
        "{v:?}"
    );
    assert_eq!(st.attendance, Attendance::None);
}

#[test]
fn t06_an_unavailable_listing_neither_proves_nor_ends() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(T0, &s, &[]),
    );
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, TaskSnapshot::Unavailable)),
            &facts(T0, &s, &[])
        )),
        Verdict::Park
    );
    assert!(matches!(st.attendance, Attendance::Intent { .. }));

    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::TaskId("b1".into()), proof()),
        &facts(T0, &s, &[]),
    );
    st.step(
        Event::TurnEnd(report(
            Tool::Claude,
            TaskSnapshot::Known(vec![shell_task("b1")]),
        )),
        &facts(T0, &s, &[]),
    );
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, TaskSnapshot::Unavailable)),
            &facts(T0 + MINUTE, &s, &[])
        )),
        Verdict::Park
    );
    assert!(matches!(st.attendance, Attendance::Standing { .. }));
}

#[test]
fn t07_another_session_changes_nothing_and_a_new_binding_drops_the_old() {
    let s = session();
    let o = other_session();
    let mut st = standing(&s);
    let before = st.clone();
    let mut stale = report(Tool::Claude, tasks(FINISHED));
    stale.session = o.clone();
    assert_eq!(
        verdict(st.step(Event::TurnEnd(stale), &facts(T0, &s, &[]))),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    let refused = st.step(
        Event::Attend(AttendRequest::Clear { session: o.clone() }),
        &facts(T0, &s, &[]),
    );
    assert_eq!(refused, Output::Attend(AttendReply::RefusedStale));
    assert_eq!(st, before);

    st.rebind(o.clone());
    assert_eq!(st.attendance, Attendance::None);
    assert_eq!(st.binding, o);
}

#[test]
fn t08_auto_off_a_question_and_a_block_come_before_attendance() {
    let s = session();
    let mut st = standing(&s);
    let mut off = facts(T0 + MINUTE, &s, &[]);
    off.auto = Ok(AutoMode::Off);
    assert_eq!(
        verdict(st.step(Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))), &off)),
        Verdict::Silent(SilentWhy::AutoOff)
    );
    let mut unreadable = facts(T0 + MINUTE, &s, &[]);
    unreadable.auto = Err("config".into());
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
            &unreadable
        )),
        Verdict::Failed("config".into())
    );

    let mut asking = facts(T0 + MINUTE, &s, &[]);
    asking.asking = true;
    assert_eq!(st.recovery_quiet(&asking), Some(Quiet::Asking));
    let mut blocked = facts(T0 + MINUTE, &s, &[]);
    blocked.blocked = true;
    blocked.asking = true;
    assert_eq!(st.recovery_quiet(&blocked), Some(Quiet::Blocked));
}

#[test]
fn t09_the_check_in_fires_once_and_reports_never_move_its_deadline() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    assert_eq!(due, T0 + 10 * MINUTE);
    assert!(effects(st.step(Event::CheckInDue, &facts(due - 1, &s, &[]))).is_empty());
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
        &facts(due - 1, &s, &[]),
    );
    assert_eq!(st.next_deadline(), Some(due));
    let fired = effects(st.step(Event::CheckInDue, &facts(due, &s, &[])));
    assert!(
        matches!(fired.as_slice(), [Effect::CheckIn { check_in, .. }] if check_in.task_id == SUBAGENT_ID)
    );
    assert!(effects(st.step(Event::CheckInDue, &facts(due + 1, &s, &[]))).is_empty());
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
        &facts(due + 1, &s, &[]),
    );
    assert_eq!(st.next_deadline(), None);
    assert!(effects(st.step(Event::CheckInDue, &facts(due + 2, &s, &[]))).is_empty());
}

#[test]
fn t10_a_restart_keeps_the_deadline_and_proves_nothing_until_the_next_stop() {
    let s = session();
    let st = standing(&s);
    let due = st.next_deadline();
    let mut back = SessionState::restored(st.persisted());
    assert_eq!(back.next_deadline(), due);
    assert_eq!(back.recovery_quiet(&facts(T0 + 2 * MINUTE, &s, &[])), None);
    assert_eq!(
        verdict(back.step(
            Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
            &facts(T0 + 2 * MINUTE, &s, &[])
        )),
        attending()
    );
    assert_eq!(
        back.recovery_quiet(&facts(T0 + 2 * MINUTE, &s, &[])),
        Some(Quiet::Attending)
    );
}

#[test]
fn t11_only_a_delivered_result_commits_and_it_commits_once() {
    let s = session();
    let owed = [OwedItem {
        identity: "gate:plan:abc".into(),
        receipts: vec!["r1".into()],
    }];
    let mut st = SessionState::new(s.clone());
    let first = verdict(st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0, &s, &owed),
    ));
    let Verdict::Deliver { presentation, .. } = first else {
        panic!("{first:?}");
    };
    assert!(
        committed(st.step(
            Event::EmitFailed(presentation.clone()),
            &facts(T0, &s, &owed)
        ))
        .is_empty()
    );
    assert_eq!(
        settled(st.step(
            Event::Delivered(presentation.clone()),
            &facts(T0, &s, &owed)
        )),
        ResultOutcome::ProtocolViolation(presentation.clone()),
        "a failure is terminal: no success can follow it"
    );
    let again = verdict(st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0, &s, &owed),
    ));
    let Verdict::Deliver {
        presentation: fresh,
        ..
    } = again
    else {
        panic!("{again:?}");
    };
    assert_ne!(
        fresh, presentation,
        "the work stays owed and is presented fresh"
    );
    let presentation = fresh;
    assert_eq!(
        committed(st.step(
            Event::Delivered(presentation.clone()),
            &facts(T0, &s, &owed)
        )),
        vec![
            Effect::CommitReceipts(vec!["r1".into()]),
            Effect::CommitRepeat(Some(RepeatRecord {
                items: vec!["gate:plan:abc".into()],
                count: 1,
                turn: None,
                high: None,
            })),
        ]
    );
    assert_eq!(
        settled(st.step(Event::Delivered(presentation), &facts(T0, &s, &owed))),
        ResultOutcome::Duplicate
    );
}

#[test]
fn t12_recovery_is_quiet_only_while_the_attendance_qualifies() {
    let s = session();
    let st = standing(&s);
    assert_eq!(
        st.recovery_quiet(&facts(T0 + 2 * MINUTE, &s, &[])),
        Some(Quiet::Attending)
    );
    assert_eq!(st.recovery_quiet(&facts(T0 + 10 * MINUTE, &s, &[])), None);

    let mut ended = st.clone();
    ended.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + 2 * MINUTE, &s, &[]),
    );
    assert_eq!(ended.recovery_quiet(&facts(T0 + 2 * MINUTE, &s, &[])), None);
}

#[test]
fn t13_codex_records_a_live_run_but_parks_on_its_own_wait() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::RunDesc("long build".into()), proof()),
        &facts(T0, &s, &[]),
    );
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Codex, TaskSnapshot::Unavailable)),
            &facts(T0 + MINUTE, &s, &[])
        )),
        Verdict::Park
    );
    assert!(matches!(st.attendance, Attendance::Intent { .. }));
    assert_eq!(st.recovery_quiet(&facts(T0 + MINUTE, &s, &[])), None);

    let mut listed = SessionState::new(s.clone());
    listed.step(
        attend(Selector::RunDesc("long build".into()), proof()),
        &facts(T0, &s, &[]),
    );
    assert_eq!(
        verdict(listed.step(
            Event::TurnEnd(report(
                Tool::Codex,
                TaskSnapshot::Known(vec![shell_task("b1")])
            )),
            &facts(T0 + MINUTE, &s, &[])
        )),
        Verdict::Park
    );
    assert!(matches!(listed.attendance, Attendance::Standing { .. }));
}

#[test]
fn t14_a_listed_shell_without_process_proof_says_clank_run_then_parks() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::TaskId("b1".into()), None),
        &facts(T0, &s, &[]),
    );
    let listing = || TaskSnapshot::Known(vec![shell_task("b1")]);
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, listing())),
            &facts(T0, &s, &[])
        )),
        Verdict::Notify(Notice::ShellNeedsRun {
            task_id: "b1".into()
        })
    );
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, listing())),
            &facts(T0 + MINUTE, &s, &[])
        )),
        Verdict::Park
    );
}

#[test]
fn t15_a_listing_without_the_shell_ends_it_even_while_its_pid_lives() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::TaskId("b1".into()), proof()),
        &facts(T0, &s, &[]),
    );
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(
                Tool::Claude,
                TaskSnapshot::Known(vec![shell_task("b1")])
            )),
            &facts(T0, &s, &[])
        )),
        Verdict::Silent(SilentWhy::Attending {
            task_id: "b1".into()
        })
    );
    st.step(
        Event::TurnEnd(report(Tool::Claude, TaskSnapshot::Known(vec![]))),
        &facts(T0 + MINUTE, &s, &[]),
    );
    assert_eq!(st.attendance, Attendance::None);

    let mut gone = SessionState::new(s.clone());
    gone.step(
        attend(Selector::TaskId("b1".into()), proof()),
        &facts(T0, &s, &[]),
    );
    let mut dead = facts(T0, &s, &[]);
    dead.liveness = &DEAD;
    assert_eq!(
        verdict(gone.step(
            Event::TurnEnd(report(
                Tool::Claude,
                TaskSnapshot::Known(vec![shell_task("b1")])
            )),
            &dead
        )),
        Verdict::Park
    );
}

#[test]
fn t16_turn_end_and_recovery_agree_on_one_deadline() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let before = facts(due - 1, &s, &[]);
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
            &before
        )),
        attending()
    );
    assert_eq!(st.recovery_quiet(&before), Some(Quiet::Attending));

    assert_eq!(
        effects(st.step(Event::CheckInDue, &facts(due, &s, &[]))).len(),
        1
    );

    let after = facts(due + 1, &s, &[]);
    assert_eq!(
        verdict(st.step(Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))), &after)),
        Verdict::Park
    );
    assert_eq!(st.recovery_quiet(&after), None);
    assert!(matches!(
        st.attendance,
        Attendance::Standing {
            since: T0,
            check_in: CheckIn::Fired,
            ..
        }
    ));
    assert!(effects(st.step(Event::CheckInDue, &after)).is_empty());
}

#[test]
fn an_unknown_task_kind_never_qualifies() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::TaskId("s8psvst3j".into()), None),
        &facts(T0, &s, &[]),
    );
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(MONITOR))),
            &facts(T0, &s, &[])
        )),
        Verdict::Park
    );
    assert!(matches!(
        st.attendance,
        Attendance::Standing {
            kind: TaskKind::Unknown(_),
            ..
        }
    ));
}

#[test]
fn alone_mode_wakes_without_presenting_and_the_monitor_delivers_once_later() {
    let s = session();
    let owed = [OwedItem {
        identity: "gate:plan:abc".into(),
        receipts: vec!["r1".into()],
    }];
    assert_eq!(
        alone_verdict(&s, &facts(T0, &s, &owed)),
        Verdict::Notify(Notice::MonitorUnavailable)
    );
    assert_eq!(
        alone_verdict(&other_session(), &facts(T0, &s, &owed)),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    let mut off = facts(T0, &s, &owed);
    off.auto = Ok(AutoMode::Off);
    assert_eq!(alone_verdict(&s, &off), Verdict::Silent(SilentWhy::AutoOff));
    let mut asking = facts(T0, &s, &owed);
    asking.asking = true;
    assert_eq!(alone_verdict(&s, &asking), Verdict::Park);
    assert_eq!(alone_verdict(&s, &facts(T0, &s, &[])), Verdict::Park);

    let mut st = SessionState::new(s.clone());
    let v = verdict(st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + MINUTE, &s, &owed),
    ));
    let Verdict::Deliver {
        presentation,
        identities,
        ..
    } = v
    else {
        panic!("{v:?}");
    };
    assert_eq!(identities, vec!["gate:plan:abc".to_string()]);
    let receipts = committed(st.step(
        Event::Delivered(presentation.clone()),
        &facts(T0, &s, &owed),
    ));
    assert_eq!(receipts[0], Effect::CommitReceipts(vec!["r1".into()]));
    assert_eq!(
        settled(st.step(Event::Delivered(presentation), &facts(T0, &s, &owed))),
        ResultOutcome::Duplicate
    );
}

#[test]
fn alone_mode_is_never_quiet_on_attendance() {
    let s = session();
    let owed = [OwedItem {
        identity: "gate:plan:abc".into(),
        receipts: Vec::new(),
    }];
    for v in [
        alone_verdict(&s, &facts(T0, &s, &owed)),
        alone_verdict(&s, &facts(T0, &s, &[])),
    ] {
        assert!(
            !matches!(v, Verdict::Silent(SilentWhy::Attending { .. })),
            "{v:?}"
        );
    }
}

#[test]
fn repeated_deliveries_count_and_a_human_turn_resets() {
    let items = vec!["a".to_string()];
    let place = |turn: &str, human| TurnPlace {
        turn: Some(turn.into()),
        position: None,
        human_turn: human,
    };
    let one = count_delivery(None, &items, &place("t1", false));
    let RepeatDecision::Counted(r1) = &one else {
        panic!()
    };
    let two = count_delivery(Some(r1), &items, &place("t1", false));
    assert_eq!(two.count(), 2);
    let reset = count_delivery(Some(r1), &items, &place("t2", true));
    assert_eq!(reset.count(), 1);
}

#[test]
fn a_check_in_respects_the_current_auto_mode() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let mut off = facts(due, &s, &[]);
    off.auto = Ok(AutoMode::Off);
    assert!(effects(st.step(Event::CheckInDue, &off)).is_empty());
}

#[test]
fn a_later_report_does_not_erase_an_emitted_presentations_receipt() {
    let s = session();
    let a = [OwedItem {
        identity: "event:a".into(),
        receipts: vec!["read:a".into()],
    }];
    let b = [OwedItem {
        identity: "event:b".into(),
        receipts: vec!["read:b".into()],
    }];
    let mut st = SessionState::new(s.clone());
    let first = verdict(st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0, &s, &a),
    ));
    let Verdict::Deliver { presentation, .. } = first else {
        panic!("{first:?}");
    };
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + 1, &s, &b),
    );
    let committed = committed(st.step(Event::Delivered(presentation), &facts(T0 + 2, &s, &b)));
    assert!(
        committed.contains(&Effect::CommitReceipts(vec!["read:a".into()])),
        "{committed:?}"
    );
}

#[test]
fn an_empty_owed_fold_forgets_the_previous_repeat_episode() {
    let s = session();
    let a = [OwedItem {
        identity: "gate:a".into(),
        receipts: vec![],
    }];
    let mut st = SessionState::new(s.clone());
    let first = verdict(st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0, &s, &a),
    ));
    let Verdict::Deliver { presentation, .. } = first else {
        panic!("{first:?}");
    };
    st.step(Event::Delivered(presentation), &facts(T0, &s, &a));
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + 1, &s, &[]),
    );
    let again = verdict(st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + 2, &s, &a),
    ));
    assert!(
        matches!(again, Verdict::Deliver { repeat: 1, .. }),
        "{again:?}"
    );
}

#[test]
fn a_check_in_held_back_by_a_gate_is_spent_and_never_rescheduled() {
    let s = session();
    for (gate, why) in [
        ("auto off", Withheld::AutoOff),
        ("asking", Withheld::Asking),
        ("blocked", Withheld::Blocked),
    ] {
        let mut st = standing(&s);
        let due = st.next_deadline().unwrap();
        let mut f = facts(due, &s, &[]);
        match gate {
            "auto off" => f.auto = Ok(AutoMode::Off),
            "asking" => f.asking = true,
            _ => f.blocked = true,
        }
        assert!(effects(st.step(Event::CheckInDue, &f)).is_empty(), "{gate}");
        assert!(
            matches!(st.attendance, Attendance::Standing { check_in: CheckIn::Withheld(w), .. } if w == why),
            "{gate}"
        );
        assert_eq!(st.next_deadline(), None, "{gate}");
        assert!(
            effects(st.step(Event::CheckInDue, &facts(due + 1, &s, &[]))).is_empty(),
            "{gate}: the gate lifting does not re-arm it"
        );
    }
}

#[test]
fn an_unreadable_config_or_role_lets_the_check_in_through() {
    let s = session();
    for broken in ["config", "role"] {
        let mut st = standing(&s);
        let due = st.next_deadline().unwrap();
        let mut f = facts(due, &s, &[]);
        if broken == "config" {
            f.auto = Err("bad config".into());
        } else {
            f.role = Err("no role".into());
        }
        assert_eq!(effects(st.step(Event::CheckInDue, &f)).len(), 1, "{broken}");
    }
}

#[test]
fn an_attendance_that_never_quieted_a_stop_owes_no_check_in() {
    let s = session();
    let mut unknown = SessionState::new(s.clone());
    unknown.step(
        attend(Selector::TaskId("s8psvst3j".into()), None),
        &facts(T0, &s, &[]),
    );
    unknown.step(
        Event::TurnEnd(report(Tool::Claude, tasks(MONITOR))),
        &facts(T0, &s, &[]),
    );
    assert_eq!(unknown.next_deadline(), None);
    assert!(effects(unknown.step(Event::CheckInDue, &facts(T0 + 11 * MINUTE, &s, &[]))).is_empty());

    let mut codex = SessionState::new(s.clone());
    codex.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(T0, &s, &[]),
    );
    codex.step(
        Event::TurnEnd(report(Tool::Codex, tasks(RUNNING))),
        &facts(T0, &s, &[]),
    );
    assert_eq!(codex.next_deadline(), None);
    assert!(effects(codex.step(Event::CheckInDue, &facts(T0 + 11 * MINUTE, &s, &[]))).is_empty());
}

fn owed(identity: &str, receipt: &str) -> [OwedItem; 1] {
    [OwedItem {
        identity: identity.into(),
        receipts: vec![receipt.into()],
    }]
}

fn deliver(st: &mut SessionState, f: &Facts<'_>) -> (PresentationId, u32) {
    match verdict(st.step(Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))), f)) {
        Verdict::Deliver {
            presentation,
            repeat,
            ..
        } => (presentation, repeat),
        other => panic!("{other:?}"),
    }
}

#[test]
fn work_owed_while_a_presentation_is_in_flight_waits_for_its_result() {
    let s = session();
    let a = owed("event:a", "read:a");
    let b = owed("event:b", "read:b");
    let mut st = SessionState::new(s.clone());
    let (pa, _) = deliver(&mut st, &facts(T0, &s, &a));
    let parked = report(Tool::Claude, tasks(FINISHED));
    assert_eq!(
        verdict(st.step(Event::TurnEnd(parked.clone()), &facts(T0 + 1, &s, &b))),
        Verdict::Park
    );
    assert_eq!(
        verdict(st.step(Event::TurnEnd(parked.clone()), &facts(T0 + 1, &s, &a))),
        Verdict::Park,
        "the same work in flight to another hook is not handed out twice"
    );
    let receipts = committed(st.step(Event::Delivered(pa.clone()), &facts(T0 + 2, &s, &b)));
    assert_eq!(receipts[0], Effect::CommitReceipts(vec!["read:a".into()]));
    let (pb, repeat) = deliver(&mut st, &facts(T0 + 3, &s, &b));
    assert_ne!(pb, pa);
    assert_eq!(repeat, 1);
    assert_eq!(
        settled(st.step(Event::Delivered(pa.clone()), &facts(T0 + 4, &s, &b))),
        ResultOutcome::Duplicate,
        "a repeat of A's settled result commits nothing while B is in flight"
    );
    assert_eq!(
        committed(st.step(Event::Delivered(pb), &facts(T0 + 4, &s, &b)))[0],
        Effect::CommitReceipts(vec!["read:b".into()])
    );
}

#[test]
fn a_presentation_id_names_one_payload() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    let (pa, _) = deliver(&mut st, &facts(T0, &s, &a));
    let before = st.outstanding.clone().unwrap();

    let mut human = report(Tool::Claude, tasks(FINISHED));
    human.place = TurnPlace {
        turn: Some("later".into()),
        position: Some(99),
        human_turn: true,
    };
    let changed = owed("event:a", "read:a2");
    assert_eq!(
        verdict(st.step(Event::TurnEnd(human), &facts(T0 + 1, &s, &changed))),
        Verdict::Park
    );
    assert_eq!(
        st.outstanding.clone().unwrap(),
        before,
        "nothing rewrites a presentation in flight"
    );
    assert_eq!(
        committed(st.step(Event::Delivered(pa.clone()), &facts(T0 + 1, &s, &changed)))[0],
        Effect::CommitReceipts(vec!["read:a".into()]),
        "it commits the receipts it was made with"
    );
    let (pc, _) = deliver(&mut st, &facts(T0 + 2, &s, &changed));
    assert_ne!(pc, pa);
}

#[test]
fn every_result_resolves_its_presentation_once() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    let (pa, _) = deliver(&mut st, &facts(T0, &s, &a));
    assert!(committed(st.step(Event::EmitFailed(pa.clone()), &facts(T0, &s, &a))).is_empty());
    assert_eq!(
        settled(st.step(Event::EmitFailed(pa.clone()), &facts(T0, &s, &a))),
        ResultOutcome::Duplicate
    );
    assert_eq!(
        settled(st.step(Event::Delivered(pa.clone()), &facts(T0, &s, &a))),
        ResultOutcome::ProtocolViolation(pa.clone())
    );

    let (pb, _) = deliver(&mut st, &facts(T0 + 1, &s, &a));
    assert_ne!(pb, pa);
    assert_eq!(
        committed(st.step(Event::Delivered(pb.clone()), &facts(T0 + 1, &s, &a)))[0],
        Effect::CommitReceipts(vec!["read:a".into()]),
        "fail, fresh, delivered: the receipt commits exactly once"
    );
    assert_eq!(
        settled(st.step(Event::Delivered(pb.clone()), &facts(T0 + 1, &s, &a))),
        ResultOutcome::Duplicate
    );
    assert_eq!(
        settled(st.step(Event::EmitFailed(pb.clone()), &facts(T0 + 1, &s, &a))),
        ResultOutcome::ProtocolViolation(pb)
    );
}

#[test]
fn a_restart_fails_the_presentation_its_connection_carried() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    let (pa, _) = deliver(&mut st, &facts(T0, &s, &a));

    let mut back = SessionState::restored(st.persisted());
    assert!(back.outstanding.is_none());
    assert_eq!(
        settled(back.step(Event::Delivered(pa.clone()), &facts(T0, &s, &a))),
        ResultOutcome::ProtocolViolation(pa.clone()),
        "its connection died with the old monitor"
    );
    let (again, _) = deliver(&mut back, &facts(T0 + 1, &s, &a));
    assert_ne!(again, pa);
    assert_eq!(
        committed(back.step(Event::Delivered(again.clone()), &facts(T0 + 1, &s, &a))).len(),
        2
    );
    assert_eq!(
        settled(back.step(Event::Delivered(again), &facts(T0 + 1, &s, &a))),
        ResultOutcome::Duplicate
    );
}

#[test]
fn a_late_result_for_failed_work_cannot_commit_after_new_work() {
    let s = session();
    let a = owed("event:a", "read:a");
    let b = owed("event:b", "read:b");
    let mut st = SessionState::new(s.clone());
    let (pa, _) = deliver(&mut st, &facts(T0, &s, &a));
    st.step(Event::EmitFailed(pa.clone()), &facts(T0 + 1, &s, &a));
    let (pb, _) = deliver(&mut st, &facts(T0 + 2, &s, &b));
    assert_ne!(pa, pb);
    assert_eq!(
        settled(st.step(Event::Delivered(pa.clone()), &facts(T0 + 3, &s, &b))),
        ResultOutcome::ProtocolViolation(pa),
        "no connection can answer for a presentation after its failure"
    );
    assert_eq!(st.outstanding.as_ref().map(|p| &p.id), Some(&pb));
    assert_eq!(
        committed(st.step(Event::Delivered(pb), &facts(T0 + 4, &s, &b)))[0],
        Effect::CommitReceipts(vec!["read:b".into()])
    );
}

#[test]
fn a_failed_presentation_cannot_revive_a_broken_repeat_episode() {
    let s = session();
    let a = owed("gate:a", "r");
    for delayed_success in [false, true] {
        let mut st = SessionState::new(s.clone());
        let (p1, _) = deliver(&mut st, &facts(T0, &s, &a));
        st.step(Event::Delivered(p1), &facts(T0, &s, &a));
        let (p2, two) = deliver(&mut st, &facts(T0 + 1, &s, &a));
        assert_eq!(two, 2);
        st.step(Event::EmitFailed(p2.clone()), &facts(T0 + 1, &s, &a));
        st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
            &facts(T0 + 2, &s, &[]),
        );
        assert!(st.repeat.is_none());
        let (p3, again) = deliver(&mut st, &facts(T0 + 3, &s, &a));
        assert_eq!(
            again, 1,
            "the failed payload belonged to the previous episode"
        );
        if delayed_success {
            st.step(Event::Delivered(p2), &facts(T0 + 4, &s, &a));
            st.step(Event::Delivered(p3), &facts(T0 + 5, &s, &a));
            assert_eq!(st.repeat.as_ref().unwrap().count, 1);
        }
    }
}

#[test]
fn serialised_deliveries_of_the_same_work_each_count() {
    let s = session();
    let a = owed("gate:a", "r");
    let mut st = SessionState::new(s.clone());
    let mut stated = Vec::new();
    for i in 0..8 {
        let (p, repeat) = deliver(&mut st, &facts(T0 + i, &s, &a));
        assert_eq!(
            verdict(st.step(
                Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
                &facts(T0 + i, &s, &a)
            )),
            Verdict::Park
        );
        st.step(Event::Delivered(p), &facts(T0 + i, &s, &a));
        stated.push(repeat);
    }
    assert_eq!(stated, (1..=8).collect::<Vec<_>>());
    assert_eq!(st.repeat.as_ref().unwrap().count, 8);
}

#[test]
fn only_deliveries_actually_emitted_are_counted() {
    let s = session();
    let a = owed("gate:a", "r");
    let at = |pos| {
        let mut r = report(Tool::Claude, tasks(FINISHED));
        r.place.position = Some(pos);
        r
    };
    for order in [[true, false], [false, true], [false, false]] {
        let mut st = SessionState::new(s.clone());
        for (pos, ok) in [(1, true), (2, true), (3, order[0]), (4, order[1])] {
            let Verdict::Deliver { presentation, .. } =
                verdict(st.step(Event::TurnEnd(at(pos)), &facts(T0, &s, &a)))
            else {
                panic!("{order:?}");
            };
            let e = if ok {
                Event::Delivered(presentation)
            } else {
                Event::EmitFailed(presentation)
            };
            st.step(e, &facts(T0, &s, &a));
        }
        let successes = order.iter().filter(|ok| **ok).count() as u32;
        assert_eq!(
            st.repeat.as_ref().map(|r| r.count),
            Some(2 + successes),
            "{order:?}"
        );
    }
}

#[test]
fn a_stop_already_counted_is_not_counted_again() {
    let s = session();
    let a = owed("gate:a", "r");
    let at = |pos| {
        let mut r = report(Tool::Claude, tasks(FINISHED));
        r.place.position = Some(pos);
        r
    };
    let mut st = SessionState::new(s.clone());
    for (pos, stated) in [(10, 1), (20, 2), (10, 2), (10, 2)] {
        let Verdict::Deliver {
            presentation,
            repeat,
            ..
        } = verdict(st.step(Event::TurnEnd(at(pos)), &facts(T0, &s, &a)))
        else {
            panic!()
        };
        assert_eq!(repeat, stated, "at {pos}");
        st.step(Event::Delivered(presentation), &facts(T0, &s, &a));
    }
    assert_eq!(st.repeat.as_ref().unwrap().count, 2);
}

#[test]
fn a_late_result_is_judged_by_the_work_owed_when_it_arrives() {
    let s = session();
    let a = owed("gate:a", "r");
    let b = owed("gate:b", "r");
    let mut st = SessionState::new(s.clone());
    let (p1, _) = deliver(&mut st, &facts(T0, &s, &a));
    st.step(Event::Delivered(p1), &facts(T0, &s, &a));
    let (p2, two) = deliver(&mut st, &facts(T0 + 1, &s, &a));
    assert_eq!(two, 2);
    let receipts = committed(st.step(Event::Delivered(p2), &facts(T0 + 2, &s, &b)));
    assert_eq!(receipts.last(), Some(&Effect::CommitRepeat(None)));
    assert_eq!(st.repeat, None);
    assert_eq!(deliver(&mut st, &facts(T0 + 3, &s, &a)).1, 1);

    let mut still = SessionState::new(s.clone());
    let (p, _) = deliver(&mut still, &facts(T0, &s, &a));
    still.step(Event::Delivered(p), &facts(T0 + 1, &s, &a));
    assert_eq!(
        still.repeat.as_ref().unwrap().count,
        1,
        "the same work still owed keeps it"
    );
}

#[test]
fn a_live_result_cannot_restore_an_episode_seen_to_end() {
    let s = session();
    let a = owed("gate:a", "r");
    let mut st = SessionState::new(s.clone());
    let (p1, _) = deliver(&mut st, &facts(T0, &s, &a));
    st.step(Event::Delivered(p1), &facts(T0, &s, &a));
    let (p2, two) = deliver(&mut st, &facts(T0 + 1, &s, &a));
    assert_eq!(two, 2);
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + 2, &s, &[]),
    );
    assert!(
        st.repeat.is_none(),
        "the observed empty set ended the episode"
    );
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + 3, &s, &a),
    );
    let result = committed(st.step(Event::Delivered(p2), &facts(T0 + 4, &s, &a)));
    assert!(result.contains(&Effect::CommitReceipts(vec!["r".into()])));
    let (_, next) = deliver(&mut st, &facts(T0 + 5, &s, &a));
    assert_eq!(
        next, 1,
        "a live success commits its receipts but not a broken episode's count"
    );
}

#[test]
fn an_unbroken_live_result_keeps_counting() {
    let s = session();
    let a = owed("gate:a", "r");
    let mut st = SessionState::new(s.clone());
    let (p1, _) = deliver(&mut st, &facts(T0, &s, &a));
    st.step(Event::Delivered(p1), &facts(T0, &s, &a));
    let (p2, two) = deliver(&mut st, &facts(T0 + 1, &s, &a));
    assert_eq!(two, 2);
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + 2, &s, &a),
    );
    st.step(Event::Delivered(p2), &facts(T0 + 3, &s, &a));
    let (_, next) = deliver(&mut st, &facts(T0 + 4, &s, &a));
    assert_eq!(next, 3);
}

#[test]
fn a_silence_needs_a_route_that_can_check_in() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(T0, &s, &[]),
    );
    let mut routeless = facts(T0 + MINUTE, &s, &[]);
    routeless.check_in_route = Err("its version is not known yet".into());
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
            &routeless
        )),
        Verdict::Park
    );
    assert!(matches!(
        st.attendance,
        Attendance::Standing {
            check_in: CheckIn::Unarmed,
            ..
        }
    ));
    assert_eq!(st.next_deadline(), None, "no silence, so no check-in owed");
    assert_eq!(st.recovery_quiet(&routeless), None);
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
            &facts(T0 + 2 * MINUTE, &s, &[])
        )),
        attending(),
        "the route back, the next Stop is quiet"
    );
}

#[test]
fn an_undeliverable_check_in_is_recorded_and_recovery_takes_over() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let check_in = fired(&mut st, due);
    st.step(
        Event::CheckInUndeliverable {
            check_in,
            why: "no pane was found".into(),
        },
        &facts(due, &s, &[]),
    );
    assert!(matches!(
        &st.attendance,
        Attendance::Standing { check_in: CheckIn::Undeliverable(why), .. } if why == "no pane was found"
    ));
    assert_eq!(st.next_deadline(), None);
    assert_eq!(st.recovery_quiet(&facts(due + 1, &s, &[])), None);
}

#[test]
fn a_parked_report_resolves_nothing_and_never_silences() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    let parked = report(Tool::Claude, tasks(FINISHED));
    assert_eq!(
        verdict(st.step(Event::TurnEnd(parked.clone()), &facts(T0, &s, &[]))),
        Verdict::Park
    );
    st.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(T0 + 1, &s, &[]),
    );
    assert_eq!(
        verdict(st.step(Event::Parked(parked.clone()), &facts(T0 + 2, &s, &[]))),
        Verdict::Park
    );
    assert!(
        matches!(st.attendance, Attendance::Intent { .. }),
        "an intent waits for the next Stop, never the parked one"
    );
    let a = owed("event:a", "read:a");
    assert!(matches!(
        verdict(st.step(Event::Parked(parked), &facts(T0 + 3, &s, &a))),
        Verdict::Deliver { notice: None, .. }
    ));

    let mut silenced = standing(&s);
    let mut stale = report(Tool::Claude, tasks(RUNNING));
    stale.session = other_session();
    assert_eq!(
        verdict(silenced.step(Event::Parked(stale), &facts(T0 + 2 * MINUTE, &s, &a))),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    assert!(matches!(
        verdict(silenced.step(
            Event::Parked(report(Tool::Claude, tasks(RUNNING))),
            &facts(T0 + 2 * MINUTE, &s, &a)
        )),
        Verdict::Deliver { .. }
    ));
    let mut off = facts(T0 + 2 * MINUTE, &s, &a);
    off.auto = Ok(AutoMode::Off);
    assert_eq!(
        verdict(silenced.step(Event::Parked(report(Tool::Claude, tasks(RUNNING))), &off)),
        Verdict::Silent(SilentWhy::AutoOff)
    );
}

#[test]
fn a_session_start_presents_once_and_counts_nothing() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    assert_eq!(
        verdict(st.step(Event::SessionStart(s.clone()), &facts(T0, &s, &[]))),
        Verdict::Park
    );
    let v = verdict(st.step(Event::SessionStart(s.clone()), &facts(T0, &s, &a)));
    let Verdict::Deliver {
        presentation,
        repeat: 0,
        escalated: false,
        ..
    } = v
    else {
        panic!("{v:?}");
    };
    assert_eq!(
        committed(st.step(Event::Delivered(presentation), &facts(T0, &s, &a))),
        vec![Effect::CommitReceipts(vec!["read:a".into()])]
    );
    assert_eq!(st.repeat, None);
    assert_eq!(deliver(&mut st, &facts(T0 + 1, &s, &a)).1, 1);
    assert_eq!(
        verdict(st.step(Event::SessionStart(other_session()), &facts(T0 + 2, &s, &a))),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
}

#[test]
fn an_observed_change_of_owed_work_forgets_the_count() {
    let s = session();
    let a = owed("gate:a", "r");
    let b = owed("gate:b", "r");
    let mut st = SessionState::new(s.clone());
    let (p, _) = deliver(&mut st, &facts(T0, &s, &a));
    st.step(Event::Delivered(p), &facts(T0, &s, &a));
    st.step(Event::Observed, &facts(T0 + 1, &s, &a));
    assert_eq!(st.repeat.as_ref().map(|r| r.count), Some(1));
    st.step(Event::Observed, &facts(T0 + 2, &s, &b));
    assert_eq!(st.repeat, None);
}

#[test]
fn a_run_is_resolved_by_exactly_one_live_run_carrying_its_description() {
    let s = session();
    let run = |id: &str, status: &str, cmd: &str| -> BackgroundTask {
        serde_json::from_value(serde_json::json!({
            "id": id, "status": status, "command": cmd,
        }))
        .unwrap()
    };
    let ours = "clank run --desc \"test run\" -- cargo test";
    let outcome = |listing: Vec<BackgroundTask>| {
        let mut st = SessionState::new(s.clone());
        st.step(
            attend(Selector::RunDesc("test run".into()), proof()),
            &facts(T0, &s, &[]),
        );
        let v = verdict(st.step(
            Event::TurnEnd(report(Tool::Claude, TaskSnapshot::Known(listing))),
            &facts(T0, &s, &[]),
        ));
        (v, st.attendance)
    };
    let (v, standing) = outcome(vec![run("t", "running", ours)]);
    assert_eq!(
        v,
        Verdict::Silent(SilentWhy::Attending {
            task_id: "t".into()
        })
    );
    assert!(matches!(standing, Attendance::Standing { task_id, .. } if task_id == "t"));
    assert!(matches!(
        outcome(vec![run("t", "completed", ours)]).0,
        Verdict::Notify(Notice::NoMatch { .. })
    ));
    assert!(matches!(
        outcome(vec![
            run("t", "running", ours),
            run(
                "u",
                "running",
                "clank run --desc \"test run\" -- cargo build"
            ),
        ])
        .0,
        Verdict::Notify(Notice::Ambiguous { .. })
    ));
    assert!(matches!(
        outcome(vec![run("t", "running", "echo \"test run\"")]).0,
        Verdict::Notify(Notice::NoMatch { .. })
    ));
}

#[test]
fn a_report_round_trips_with_every_task_field_kept() {
    let r = report(Tool::Claude, tasks(RUNNING));
    let wire = serde_json::to_string(&r).unwrap();
    let back: TurnEndReport = serde_json::from_str(&wire).unwrap();
    assert_eq!(back, r);
    let TaskSnapshot::Known(listed) = back.tasks else {
        panic!("known");
    };
    assert_eq!(listed[0].raw["agent_type"], "general-purpose");
    let unavailable = report(Tool::Codex, TaskSnapshot::Unavailable);
    let back: TurnEndReport =
        serde_json::from_str(&serde_json::to_string(&unavailable).unwrap()).unwrap();
    assert_eq!(back.tasks, TaskSnapshot::Unavailable);
}

/// A Stop placed at or before one already counted, for other items, is a
/// capture overtaken by newer deliveries: it states no count and changes
/// none. The same items there are a duplicate, stating their count.
#[test]
fn an_overtaken_capture_states_no_count_and_changes_none() {
    let at = |position| TurnPlace {
        turn: None,
        position: Some(position),
        human_turn: false,
    };
    let b = RepeatRecord {
        items: vec!["b".into()],
        count: 4,
        turn: None,
        high: Some(6),
    };
    let a = vec!["a".to_string()];
    assert_eq!(
        count_delivery(Some(&b), &a, &at(2)),
        RepeatDecision::Superseded
    );
    assert_eq!(
        count_delivery(Some(&b), &a, &at(6)),
        RepeatDecision::Superseded,
        "other items at the same place"
    );
    assert_eq!(
        count_delivery(Some(&b), &["b".to_string()], &at(5)),
        RepeatDecision::Duplicate(4)
    );
}

/// Only a turn the record positively calls a person's resets the count,
/// and only once it is newer than the last delivery's.
#[test]
fn only_a_known_persons_newer_turn_resets() {
    let items = vec!["a".to_string()];
    let mut prev: Option<RepeatRecord> = None;
    let mut stated = Vec::new();
    for (turn, human, position) in [
        ("p1", false, 1),
        ("p2", false, 2),
        ("p3", true, 3),
        ("p4", false, 4),
    ] {
        let place = TurnPlace {
            turn: Some(turn.into()),
            position: Some(position),
            human_turn: human,
        };
        let RepeatDecision::Counted(next) = count_delivery(prev.as_ref(), &items, &place) else {
            panic!("{turn}");
        };
        stated.push(next.count);
        prev = Some(next);
    }
    assert_eq!(stated, [1, 2, 1, 2]);
}

/// Fire `st`'s check-in at `due`, and say which attendance it was for.
fn fired(st: &mut SessionState, due: Ms) -> CheckInRef {
    let s = st.binding.clone();
    match effects(st.step(Event::CheckInDue, &facts(due, &s, &[]))).as_slice() {
        [Effect::CheckIn { check_in, .. }] => check_in.clone(),
        other => panic!("no check-in fired: {other:?}"),
    }
}

#[test]
fn a_lost_check_in_is_recorded_only_against_the_attendance_it_was_for() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let old = fired(&mut st, due);
    st.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(due + 1, &s, &[]),
    );
    st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))),
        &facts(due + 2, &s, &[]),
    );
    let replaced = st.attendance.clone();
    st.step(
        Event::CheckInUndeliverable {
            check_in: old.clone(),
            why: "typing it did not start".into(),
        },
        &facts(due + 3, &s, &[]),
    );
    assert_eq!(
        st.attendance, replaced,
        "the replacement keeps its own check-in"
    );
    assert_eq!(st.next_deadline(), Some(due + 1 + 10 * MINUTE));
}

#[test]
fn the_last_look_refuses_a_check_in_whose_attendance_or_gates_changed() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let ours = fired(&mut st, due);
    let now = facts(due, &s, &[]);
    assert_eq!(st.may_type_check_in(&ours, &now), Ok(()));
    let off = Facts {
        auto: Ok(AutoMode::Off),
        ..facts(due, &s, &[])
    };
    let asking = Facts {
        asking: true,
        ..facts(due, &s, &[])
    };
    let blocked = Facts {
        blocked: true,
        ..facts(due, &s, &[])
    };
    let other = other_session();
    let rebound = facts(due, &other, &[]);
    for (gate, why) in [
        (&off, "auto turned off"),
        (&asking, "a question is pending"),
        (&blocked, "a block is pending"),
        (&rebound, "the session no longer holds the label"),
    ] {
        assert_eq!(st.may_type_check_in(&ours, gate), Err(why.to_string()));
    }

    let mut cleared = st.clone();
    cleared.step(
        Event::Attend(AttendRequest::Clear { session: s.clone() }),
        &now,
    );
    let mut replaced = st.clone();
    replaced.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(due + 1, &s, &[]),
    );
    for st in [cleared, replaced] {
        assert_eq!(
            st.may_type_check_in(&ours, &facts(due + 1, &s, &[])),
            Err("the attendance it was for was cleared or replaced".to_string())
        );
    }
}

#[test]
fn a_native_handoff_asks_for_one_wait_through_the_drive_gate() {
    let s = session();
    let handoff = |wait_running, continued| Handoff {
        session: s.clone(),
        wait_running,
        continued,
    };
    let on = facts(T0, &s, &[]);
    assert_eq!(
        handoff_verdict(&handoff(false, false), &on),
        Verdict::Notify(Notice::ArmWait)
    );
    for (running, continued) in [(true, false), (false, true)] {
        assert_eq!(
            handoff_verdict(&handoff(running, continued), &on),
            Verdict::Silent(SilentWhy::WaitArmed)
        );
    }
    let off = Facts {
        auto: Ok(AutoMode::Off),
        ..facts(T0, &s, &[])
    };
    assert_eq!(
        handoff_verdict(&handoff(false, false), &off),
        Verdict::Silent(SilentWhy::AutoOff)
    );
    let unroled = Facts {
        role: Err("not on the roster".into()),
        ..facts(T0, &s, &[])
    };
    assert_eq!(
        handoff_verdict(&handoff(false, false), &unroled),
        Verdict::Failed("not on the roster".into())
    );
    let other = other_session();
    assert_eq!(
        handoff_verdict(&handoff(false, false), &facts(T0, &other, &[])),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
}
