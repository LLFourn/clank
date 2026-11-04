use super::*;
use crate::hook_io::HookInput;

const RUNNING: &str = include_str!("../../tests/fixtures/stop_hook/stop_subagent_running.json");
const FINISHED: &str = include_str!("../../tests/fixtures/stop_hook/stop_subagent_finished.json");
const MONITOR: &str = include_str!("../../tests/fixtures/stop_hook/stop_monitor_task_running.json");

const SUBAGENT_ID: &str = "a25eee2eef81caf3e";
const SUBAGENT_NAME: &str = "capture-probe sleeper";
const UNCOVERED_SUBAGENT: &str = "uncovered:a25eee2eef81caf3e";
const MONITOR_NAME: &str =
    "live updates for artifact https://claude.ai/artifact/REDACTED (re-armed on session resume)";
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
        user_command: false,
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

/// A qualifying Stop with nothing deliverable parks, so later work can
/// still wake the agent.
fn attending() -> Verdict {
    Verdict::Park
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

fn entry(st: &SessionState) -> &Entry {
    match st.attendances.as_slice() {
        [only] => only,
        other => panic!("not one entry: {other:?}"),
    }
}

fn since(st: &SessionState) -> Ms {
    entry(st).since
}

fn notices(v: &Verdict) -> &[Notice] {
    match v {
        Verdict::Notify(n) | Verdict::Deliver { notices: n, .. } => n,
        _ => &[],
    }
}

fn uncovered(v: &Verdict) -> Vec<&str> {
    notices(v)
        .iter()
        .flat_map(|n| match n {
            Notice::Uncovered(tasks) => tasks.iter().map(|t| t.task_id.as_str()).collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn clear_all() -> Event {
    Event::Attend(AttendRequest::Clear {
        session: session(),
        which: Clearing::All,
    })
}

fn check_ins(fx: Vec<Effect>) -> Vec<CheckInRef> {
    match fx.as_slice() {
        [Effect::CheckIn(lines)] => lines.iter().map(|l| l.check_in.clone()).collect(),
        other => panic!("not one check-in: {other:?}"),
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
    let e = entry(&st);
    assert_eq!(
        e.resolved,
        Some(Resolved {
            task_id: SUBAGENT_ID.into(),
            kind: TaskKind::Subagent
        })
    );
    assert_eq!(e.check_in, CheckIn::Armed);
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
    assert!(st.attendances.is_empty());
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
    let [Notice::NoMatch { live, .. }, ..] = notices(&v) else {
        panic!("{v:?}");
    };
    assert_eq!(live[0].description.as_deref(), Some(SUBAGENT_NAME));
    assert!(st.attendances.is_empty());

    let mut twins = shell_task("one");
    twins.description = Some("same".into());
    let mut other = shell_task("two");
    other.description = Some("same".into());
    st.step(
        attend(Selector::Name("same".into()), None),
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
        matches!(notices(&v), [Notice::Ambiguous { matches, .. }, ..] if matches.len() == 2),
        "{v:?}"
    );
    assert!(st.attendances.is_empty());
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
    assert!(entry(&st).resolved.is_none());

    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::RunDesc("long build".into()), proof()),
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
    assert!(entry(&st).resolved.is_some());
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
        Event::Attend(AttendRequest::Clear {
            session: o.clone(),
            which: Clearing::All,
        }),
        &facts(T0, &s, &[]),
    );
    assert_eq!(refused, Output::Attend(AttendReply::RefusedStale));
    assert_eq!(st, before);

    st.rebind(o.clone());
    assert!(st.attendances.is_empty());
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
        matches!(fired.as_slice(), [Effect::CheckIn(lines)] if lines.len() == 1 && lines[0].task_id == SUBAGENT_ID)
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
    assert!(entry(&st).resolved.is_none());
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
    assert!(entry(&listed).resolved.is_some());
}

#[test]
fn t15_a_listing_without_the_shell_ends_it_even_while_its_pid_lives() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::RunDesc("long build".into()), proof()),
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
        Verdict::Park
    );
    st.step(
        Event::TurnEnd(report(Tool::Claude, TaskSnapshot::Known(vec![]))),
        &facts(T0 + MINUTE, &s, &[]),
    );
    assert!(st.attendances.is_empty());
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
    let v = verdict(st.step(Event::TurnEnd(report(Tool::Claude, tasks(RUNNING))), &after));
    assert_eq!(
        uncovered(&v),
        [SUBAGENT_ID],
        "past its deadline it covers nothing"
    );
    assert_eq!(st.recovery_quiet(&after), None);
    assert_eq!((since(&st), &entry(&st).check_in), (T0, &CheckIn::Fired));
    assert!(effects(st.step(Event::CheckInDue, &after)).is_empty());
}

#[test]
fn an_unknown_task_kind_never_qualifies() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(
        attend(Selector::Name(MONITOR_NAME.into()), None),
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
        entry(&st).resolved,
        Some(Resolved {
            kind: TaskKind::Unknown(_),
            ..
        })
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
        Verdict::Notify(vec![Notice::MonitorUnavailable])
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
        assert!(!matches!(v, Verdict::Silent(_)), "{v:?}");
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
        assert!(entry(&st).check_in == CheckIn::Withheld(why), "{gate}");
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
        attend(Selector::Name(MONITOR_NAME.into()), None),
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

/// No check-in route is consulted: an attendance on a session whose
/// typed check-in could never reach it quiets the work it received, and
/// its deadline alone ends that, even when the check-in it fires is lost.
#[test]
fn received_work_is_quiet_without_a_check_in_route_until_the_deadline() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then_attending(&s, &a);
    let deadline = st.next_deadline().expect("the quiet arms its check-in");
    let before = facts(deadline - 1, &s, &a);
    assert_eq!(st.recovery_quiet(&before), Some(Quiet::Attending));
    assert_eq!(
        verdict(st.step(Event::Parked(report(Tool::Claude, tasks(RUNNING))), &before)),
        Verdict::Park
    );

    let at = facts(deadline, &s, &a);
    let check_ins = check_ins(effects(st.step(Event::CheckInDue, &at)));
    st.step(
        Event::CheckInUndeliverable {
            check_ins,
            why: "its pane was not found".into(),
        },
        &at,
    );
    assert_eq!(st.recovery_quiet(&at), None);
    let (_, identities) = presented(verdict(
        st.step(Event::Parked(report(Tool::Claude, tasks(RUNNING))), &at),
    ));
    assert_eq!(
        identities,
        ["event:a"],
        "past its deadline the attendance quiets nothing, check-in lost or not"
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
            check_ins: vec![check_in],
            why: "no pane was found".into(),
        },
        &facts(due, &s, &[]),
    );
    assert_eq!(
        entry(&st).check_in,
        CheckIn::Undeliverable("no pane was found".into())
    );
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
        entry(&st).resolved.is_none(),
        "an intent waits for the next Stop, never the parked one"
    );
    let a = owed("event:a", "read:a");
    assert!(matches!(
        verdict(st.step(Event::Parked(parked), &facts(T0 + 3, &s, &a))),
        Verdict::Deliver { notices, .. } if notices.is_empty()
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
        (v, st.attendances)
    };
    let (v, standing) = outcome(vec![run("t", "running", ours)]);
    assert_eq!(v, Verdict::Park);
    assert!(matches!(standing.as_slice(), [e] if e.task() == Some("t")));
    assert!(matches!(
        notices(&outcome(vec![run("t", "completed", ours)]).0),
        [Notice::NoMatch { .. }]
    ));
    assert!(matches!(
        notices(
            &outcome(vec![
                run("t", "running", ours),
                run(
                    "u",
                    "running",
                    "clank run --desc \"test run\" -- cargo build"
                ),
            ])
            .0
        ),
        [Notice::Ambiguous { .. }, Notice::Uncovered(_)]
    ));
    assert!(matches!(
        notices(&outcome(vec![run("t", "running", "echo \"test run\"")]).0),
        [Notice::NoMatch { .. }, Notice::Uncovered(_)]
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
    match check_ins(effects(st.step(Event::CheckInDue, &facts(due, &s, &[])))).as_slice() {
        [one] => one.clone(),
        other => panic!("not one entry checked in on: {other:?}"),
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
    let renewed = st.attendances.clone();
    st.step(
        Event::CheckInUndeliverable {
            check_ins: vec![old.clone()],
            why: "typing it did not start".into(),
        },
        &facts(due + 3, &s, &[]),
    );
    assert_eq!(
        st.attendances, renewed,
        "the renewal keeps its own check-in"
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
    let ours = [ours];
    assert!(
        matches!(st.may_type_check_in(&ours, &now).as_deref(), Ok([line]) if line.check_in == ours[0])
    );
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
    cleared.step(clear_all(), &now);
    let mut renewed = st.clone();
    renewed.step(
        attend(Selector::Name(SUBAGENT_NAME.into()), None),
        &facts(due + 1, &s, &[]),
    );
    renewed.step(running_stop(), &facts(due + 1, &s, &[]));
    for st in [cleared, renewed] {
        assert_eq!(
            st.may_type_check_in(&ours, &facts(due + 1, &s, &[])),
            Err("the attendance it was for was cleared or renewed".to_string())
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
        Verdict::Notify(vec![Notice::ArmWait])
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

fn presented(v: Verdict) -> (PresentationId, Vec<String>) {
    match v {
        Verdict::Deliver {
            presentation,
            identities,
            ..
        } => (presentation, identities),
        other => panic!("not a delivery: {other:?}"),
    }
}

fn running_stop() -> Event {
    Event::TurnEnd(report(Tool::Claude, tasks(RUNNING)))
}

fn attend_subagent() -> Event {
    attend(Selector::Name(SUBAGENT_NAME.into()), None)
}

/// `item` delivered and received, then the captured subagent attended and
/// proven by a Stop that parks.
fn received_then_attending(s: &SessionId, item: &[OwedItem]) -> SessionState {
    let mut st = SessionState::new(s.clone());
    let (p, _) = deliver(&mut st, &facts(T0, s, item));
    committed(st.step(Event::Delivered(p), &facts(T0, s, item)));
    st.step(attend_subagent(), &facts(T0, s, item));
    assert_eq!(
        verdict(st.step(running_stop(), &facts(T0 + MINUTE, s, item))),
        Verdict::Park,
        "received work is quiet"
    );
    st
}

#[test]
fn work_that_arrived_before_the_first_qualifying_stop_is_delivered() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    st.step(attend_subagent(), &facts(T0, &s, &[]));
    let (_, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + MINUTE, &s, &a)),
    ));
    assert_eq!(identities, ["event:a"]);
    assert_eq!(entry(&st).check_in, CheckIn::Armed);
}

#[test]
fn received_work_stays_quiet_and_new_work_still_wakes() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then_attending(&s, &a);
    assert_eq!(
        verdict(st.step(running_stop(), &facts(T0 + 2 * MINUTE, &s, &a))),
        Verdict::Park
    );

    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let (pb, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 3 * MINUTE, &s, &ab)),
    ));
    assert_eq!(identities, ["event:b"], "only what was not received");
    assert_eq!(
        committed(st.step(Event::Delivered(pb), &facts(T0 + 3 * MINUTE, &s, &ab)))[0],
        Effect::CommitReceipts(vec!["read:b".into()])
    );
    assert_eq!(
        verdict(st.step(running_stop(), &facts(T0 + 4 * MINUTE, &s, &ab))),
        Verdict::Park,
        "work delivered while attending is received too"
    );
}

#[test]
fn work_whose_delivery_failed_or_was_lost_stays_deliverable() {
    let s = session();
    let a = owed("event:a", "read:a");
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let mut st = received_then_attending(&s, &a);

    let (failed, _) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 2 * MINUTE, &s, &ab)),
    ));
    st.step(Event::EmitFailed(failed), &facts(T0 + 2 * MINUTE, &s, &ab));
    let (lost, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 3 * MINUTE, &s, &ab)),
    ));
    assert_eq!(identities, ["event:b"], "a failed emit received nothing");

    let restarted: Persisted =
        serde_json::from_str(&serde_json::to_string(&st.persisted()).unwrap()).unwrap();
    let mut st = SessionState::restored(restarted);
    let (_, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 4 * MINUTE, &s, &ab)),
    ));
    assert_eq!(identities, ["event:b"], "a lost result received nothing");
    assert!(matches!(
        settled(st.step(Event::Delivered(lost), &facts(T0 + 4 * MINUTE, &s, &ab))),
        ResultOutcome::ProtocolViolation(_)
    ));
    assert_eq!(st.received, ["event:a"]);
}

#[test]
fn a_result_for_another_binding_receives_nothing() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    st.step(attend_subagent(), &facts(T0, &s, &[]));
    let (p, _) = presented(verdict(st.step(running_stop(), &facts(T0, &s, &a))));
    let other = other_session();
    assert!(matches!(
        settled(st.step(Event::Delivered(p), &facts(T0, &other, &a))),
        ResultOutcome::ProtocolViolation(_)
    ));
    assert!(st.received.is_empty());
}

#[test]
fn a_parked_report_with_only_received_work_keeps_parking() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then_attending(&s, &a);
    let parked = report(Tool::Claude, tasks(RUNNING));
    for at in 2..5 {
        assert_eq!(
            verdict(st.step(
                Event::Parked(parked.clone()),
                &facts(T0 + at * MINUTE, &s, &a)
            )),
            Verdict::Park
        );
    }
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let (_, identities) = presented(verdict(
        st.step(Event::Parked(parked), &facts(T0 + 5 * MINUTE, &s, &ab)),
    ));
    assert_eq!(identities, ["event:b"]);
}

#[test]
fn a_session_start_presents_only_unreceived_work_while_attending() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then_attending(&s, &a);
    assert_eq!(
        verdict(st.step(
            Event::SessionStart(s.clone()),
            &facts(T0 + 2 * MINUTE, &s, &a)
        )),
        Verdict::Park
    );
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let (_, identities) = presented(verdict(st.step(
        Event::SessionStart(s.clone()),
        &facts(T0 + 2 * MINUTE, &s, &ab),
    )));
    assert_eq!(identities, ["event:b"]);
}

#[test]
fn clearing_the_attendance_presents_received_work_again() {
    let s = session();
    let a = owed("event:a", "read:a");
    let clear = clear_all;

    let mut st = received_then_attending(&s, &a);
    st.step(clear(), &facts(T0 + 2 * MINUTE, &s, &a));
    let (_, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 3 * MINUTE, &s, &a)),
    ));
    assert_eq!(identities, ["event:a", UNCOVERED_SUBAGENT]);

    let mut again = received_then_attending(&s, &a);
    again.step(clear(), &facts(T0 + 2 * MINUTE, &s, &a));
    again.step(attend_subagent(), &facts(T0 + 2 * MINUTE, &s, &a));
    let (_, identities) = presented(verdict(
        again.step(running_stop(), &facts(T0 + 3 * MINUTE, &s, &a)),
    ));
    assert_eq!(
        identities,
        ["event:a"],
        "a cleared attendance's quiet is not inherited by the next"
    );
}

fn subagent(id: &str, name: &str) -> BackgroundTask {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "type": "subagent",
        "status": "running",
        "description": name,
        "agent_type": "general-purpose",
    }))
    .unwrap()
}

fn listing(live: Vec<BackgroundTask>) -> Event {
    Event::TurnEnd(report(Tool::Claude, TaskSnapshot::Known(live)))
}

#[test]
fn re_attending_the_same_task_keeps_what_it_quieted() {
    let s = session();
    let a = owed("event:a", "read:a");
    let renewals: [&[Selector]; 2] = [
        &[Selector::Name(SUBAGENT_NAME.into())],
        &[
            Selector::Name(SUBAGENT_NAME.into()),
            Selector::Name(SUBAGENT_NAME.into()),
        ],
    ];
    for requests in renewals {
        let mut st = received_then_attending(&s, &a);
        for selector in requests {
            st.step(
                attend(selector.clone(), None),
                &facts(T0 + 2 * MINUTE, &s, &a),
            );
        }
        assert_eq!(
            verdict(st.step(running_stop(), &facts(T0 + 3 * MINUTE, &s, &a))),
            Verdict::Park,
            "{requests:?}"
        );
        assert_eq!(st.received, ["event:a"], "{requests:?}");
        let e = entry(&st);
        assert_eq!(
            (e.task(), e.since, &e.check_in),
            (Some(SUBAGENT_ID), T0 + 2 * MINUTE, &CheckIn::Armed),
            "{requests:?}"
        );
    }
}

#[test]
fn a_renewal_keeps_the_entry_in_force() {
    let s = session();
    let a = owed("event:a", "read:a");
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let parked = || Event::Parked(report(Tool::Claude, tasks(RUNNING)));
    {
        let selector = Selector::Name(SUBAGENT_NAME.into());
        let mut st = received_then_attending(&s, &a);
        assert_eq!(st.next_deadline(), Some(T0 + 10 * MINUTE));
        st.step(
            attend(selector.clone(), None),
            &facts(T0 + 2 * MINUTE, &s, &a),
        );
        let deadline = Some(T0 + 10 * MINUTE);
        let gap = facts(T0 + 3 * MINUTE, &s, &a);
        assert_eq!(
            st.recovery_quiet(&gap),
            Some(Quiet::Attending),
            "{selector:?}"
        );
        assert_eq!(
            verdict(st.step(parked(), &gap)),
            Verdict::Park,
            "{selector:?}"
        );
        assert_eq!(
            verdict(st.step(Event::SessionStart(s.clone()), &gap)),
            Verdict::Park,
            "{selector:?}"
        );
        assert_eq!(st.next_deadline(), deadline, "{selector:?}");

        let (_, identities) =
            presented(verdict(st.step(parked(), &facts(T0 + 4 * MINUTE, &s, &ab))));
        assert_eq!(
            identities,
            ["event:b"],
            "{selector:?}: new work still wakes"
        );
        st.step(running_stop(), &facts(T0 + 5 * MINUTE, &s, &ab));
        assert_eq!(
            st.next_deadline(),
            Some(T0 + 12 * MINUTE),
            "the listing shows it is the same task, so the request renews it"
        );
        assert_eq!(st.attendances.len(), 1);
    }
}

#[test]
fn re_attending_the_same_task_still_presents_newly_owed_work() {
    let s = session();
    let a = owed("event:a", "read:a");
    let revision = owed("commit_plan_revision:p", "rev:p");
    let both = [a[0].clone(), revision[0].clone()];
    let mut st = received_then_attending(&s, &a);
    st.step(attend_subagent(), &facts(T0 + 2 * MINUTE, &s, &both));
    let (p, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 3 * MINUTE, &s, &both)),
    ));
    assert_eq!(identities, ["commit_plan_revision:p"]);
    assert_eq!(st.received, ["event:a"], "presenting it received nothing");
    st.step(Event::EmitFailed(p), &facts(T0 + 3 * MINUTE, &s, &both));
    let (_, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 4 * MINUTE, &s, &both)),
    ));
    assert_eq!(identities, ["commit_plan_revision:p"]);
}

#[test]
fn a_renewal_refreshes_the_expectation_and_its_check_in() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then_attending(&s, &a);
    let due = st.next_deadline().unwrap();
    assert_eq!(due, T0 + 10 * MINUTE);
    let old = check_ins(effects(st.step(Event::CheckInDue, &facts(due, &s, &a))));
    assert_eq!(st.next_deadline(), None, "a fired check-in is spent");

    st.step(attend_subagent(), &facts(due + 1, &s, &a));
    assert_eq!(
        verdict(st.step(running_stop(), &facts(due + 2, &s, &a))),
        Verdict::Park,
        "the renewal quiets what was received under its new expectation"
    );
    let renewed = due + 1 + 10 * MINUTE;
    assert_eq!(st.next_deadline(), Some(renewed));

    let before = st.attendances.clone();
    st.step(
        Event::CheckInUndeliverable {
            check_ins: old.clone(),
            why: "typing it did not start".into(),
        },
        &facts(due + 3, &s, &a),
    );
    assert_eq!(
        st.attendances, before,
        "the old check-in's loss is not the renewal's"
    );
    assert!(st.may_type_check_in(&old, &facts(due + 3, &s, &a)).is_err());

    let fresh = check_ins(effects(st.step(Event::CheckInDue, &facts(renewed, &s, &a))));
    assert_eq!(
        fresh,
        [CheckInRef {
            key: EntryKey::Task(SUBAGENT_ID.into()),
            since: due + 1,
        }]
    );
}

#[test]
fn clearing_cancels_a_pending_request_with_its_attendance() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then_attending(&s, &a);
    st.step(attend_subagent(), &facts(T0 + 2 * MINUTE, &s, &a));
    st.step(clear_all(), &facts(T0 + 3 * MINUTE, &s, &a));
    assert!(st.attendances.is_empty());
    let (_, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 4 * MINUTE, &s, &a)),
    ));
    assert_eq!(identities, ["event:a", UNCOVERED_SUBAGENT]);
    assert!(st.attendances.is_empty(), "nothing was left to resolve");
}

#[test]
fn an_ended_task_presents_received_work_and_releases_it() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then_attending(&s, &a);
    let (p, identities) = presented(verdict(st.step(
        Event::TurnEnd(report(Tool::Claude, tasks(FINISHED))),
        &facts(T0 + 2 * MINUTE, &s, &a),
    )));
    assert_eq!(identities, ["event:a"]);
    assert!(st.attendances.is_empty());
    st.step(Event::EmitFailed(p), &facts(T0 + 2 * MINUTE, &s, &a));
    st.step(attend_subagent(), &facts(T0 + 2 * MINUTE, &s, &a));
    let (_, identities) = presented(verdict(
        st.step(running_stop(), &facts(T0 + 3 * MINUTE, &s, &a)),
    ));
    assert_eq!(
        identities,
        ["event:a"],
        "an ended attendance's quiet is not inherited by the next"
    );
}

#[test]
fn past_the_deadline_or_unqualified_received_work_is_deliverable() {
    let s = session();
    let a = owed("event:a", "read:a");
    let deadline = T0 + 10 * MINUTE;

    let mut st = received_then_attending(&s, &a);
    let (_, identities) = presented(verdict(st.step(
        Event::Parked(report(Tool::Claude, tasks(RUNNING))),
        &facts(deadline, &s, &a),
    )));
    assert_eq!(
        identities,
        ["event:a"],
        "a parked re-evaluation at the deadline"
    );

    let mut st = received_then_attending(&s, &a);
    let (_, identities) = presented(verdict(st.step(running_stop(), &facts(deadline, &s, &a))));
    assert_eq!(identities, ["event:a", UNCOVERED_SUBAGENT]);
    assert_eq!(
        st.next_deadline(),
        Some(deadline),
        "the deadline is unchanged"
    );

    let mut st = received_then_attending(&s, &a);
    let (_, identities) = presented(verdict(st.step(
        Event::TurnEnd(report(Tool::Codex, tasks(RUNNING))),
        &facts(T0 + 2 * MINUTE, &s, &a),
    )));
    assert_eq!(identities, ["event:a"], "no completion wake, so no quiet");
}

#[test]
fn received_work_is_scoped_to_the_binding_and_survives_a_restart() {
    let s = session();
    let a = owed("event:a", "read:a");
    let st = received_then_attending(&s, &a);
    assert_eq!(st.received, ["event:a"]);

    let mut moved = st.clone();
    moved.rebind(other_session());
    assert!(moved.received.is_empty());

    let wire = serde_json::to_value(st.persisted()).unwrap();
    let mut back = SessionState::restored(serde_json::from_value(wire.clone()).unwrap());
    assert_eq!(back.received, ["event:a"]);
    assert_eq!(
        verdict(back.step(running_stop(), &facts(T0 + 2 * MINUTE, &s, &a))),
        Verdict::Park
    );

    let mut old = wire;
    old.as_object_mut().unwrap().remove("received").unwrap();
    let mut old = SessionState::restored(serde_json::from_value(old).unwrap());
    assert!(old.received.is_empty());
    let (_, identities) = presented(verdict(
        old.step(running_stop(), &facts(T0 + 2 * MINUTE, &s, &a)),
    ));
    assert_eq!(identities, ["event:a"], "no evidence is never deaf");
}

#[test]
fn received_work_no_longer_owed_is_forgotten() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then_attending(&s, &a);
    st.step(Event::Observed, &facts(T0 + 2 * MINUTE, &s, &[]));
    assert!(st.received.is_empty());
}

#[test]
fn quieted_work_is_neither_presented_nor_counted() {
    let s = session();
    let a = owed("event:a", "read:a");
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let mut st = received_then_attending(&s, &a);
    let at = |position| {
        let mut r = report(Tool::Claude, tasks(RUNNING));
        r.place = TurnPlace {
            turn: Some(format!("turn-{position}")),
            position: Some(position),
            human_turn: false,
        };
        Event::TurnEnd(r)
    };
    assert_eq!(
        verdict(st.step(at(1), &facts(T0 + 2 * MINUTE, &s, &a))),
        Verdict::Park
    );
    let Verdict::Deliver {
        presentation,
        identities,
        repeat,
        ..
    } = verdict(st.step(at(2), &facts(T0 + 3 * MINUTE, &s, &ab)))
    else {
        panic!("b is delivered");
    };
    assert_eq!(
        (identities.as_slice(), repeat),
        (&["event:b".to_string()][..], 1)
    );
    st.step(
        Event::EmitFailed(presentation),
        &facts(T0 + 3 * MINUTE, &s, &ab),
    );
    let Verdict::Deliver { repeat, .. } = verdict(st.step(at(3), &facts(T0 + 4 * MINUTE, &s, &ab)))
    else {
        panic!("b is delivered again");
    };
    assert_eq!(repeat, 1, "the received item never ticks the count");
}

#[test]
fn attendance_changes_no_verdict_for_codex_or_opencode() {
    let s = session();
    let a = owed("event:a", "read:a");
    for tool in [Tool::Codex, Tool::OpenCode] {
        let stop = || Event::TurnEnd(report(tool, TaskSnapshot::Known(vec![shell_task("b1")])));
        let run = |attending: bool| {
            let mut st = SessionState::new(s.clone());
            let (p, _) = presented(verdict(st.step(stop(), &facts(T0, &s, &a))));
            committed(st.step(Event::Delivered(p), &facts(T0, &s, &a)));
            if attending {
                st.step(
                    attend(Selector::RunDesc("long build".into()), proof()),
                    &facts(T0, &s, &a),
                );
            }
            let v = verdict(st.step(stop(), &facts(T0 + MINUTE, &s, &a)));
            (v, st)
        };
        let (attended, st) = run(true);
        assert!(entry(&st).resolved.is_some(), "{tool:?}");
        assert_eq!(attended, run(false).0, "{tool:?}");
        assert!(
            matches!(&attended, Verdict::Deliver { identities, .. } if identities == &["event:a"]),
            "{tool:?}: {attended:?}"
        );
    }
}

#[test]
fn attendance_holds_recovery_only_while_everything_owed_was_received() {
    let s = session();
    let a = owed("event:a", "read:a");
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let st = received_then_attending(&s, &a);
    let at = T0 + 2 * MINUTE;
    assert_eq!(
        st.recovery_quiet(&facts(at, &s, &a)),
        Some(Quiet::Attending)
    );
    assert_eq!(
        st.recovery_quiet(&facts(at, &s, &[])),
        Some(Quiet::Attending)
    );
    assert_eq!(
        st.recovery_quiet(&facts(at, &s, &ab)),
        None,
        "work never received stays recoverable"
    );
}

/// A Stop that would park ends silently while a verified user command
/// waits, both as a new Stop and as a parked report stepped again; work
/// owed is still delivered first, and without the fact nothing changes.
#[test]
fn a_waiting_user_command_ends_a_park_and_nothing_else() {
    let s = session();
    fn waiting(f: Facts<'_>) -> Facts<'_> {
        Facts {
            user_command: true,
            ..f
        }
    }
    let mut st = SessionState::new(s.clone());
    let stop = report(Tool::Claude, tasks(FINISHED));
    assert_eq!(
        verdict(st.step(Event::TurnEnd(stop.clone()), &facts(T0, &s, &[]))),
        Verdict::Park
    );
    assert_eq!(
        verdict(st.step(Event::Parked(stop.clone()), &waiting(facts(T0, &s, &[])))),
        Verdict::Silent(SilentWhy::UserCommand)
    );
    assert_eq!(
        verdict(st.step(Event::TurnEnd(stop.clone()), &waiting(facts(T0, &s, &[])))),
        Verdict::Silent(SilentWhy::UserCommand)
    );
    let a = owed("event:a", "read:a");
    assert!(matches!(
        verdict(st.step(Event::TurnEnd(stop), &waiting(facts(T0, &s, &a)))),
        Verdict::Deliver { .. }
    ));
}

fn run_task(id: &str, desc: &str) -> BackgroundTask {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "type": "shell",
        "status": "running",
        "description": "Run the full integration test harness",
        "command": format!("clank run --desc \"{desc}\" -- cargo test"),
    }))
    .unwrap()
}

fn proof_of(pid: i32) -> ShellProof {
    ShellProof {
        pid,
        token: format!("mac1:{pid}:1"),
    }
}

fn attend_run(desc: &str, pid: i32) -> Event {
    Event::Attend(AttendRequest::Attend {
        session: session(),
        selector: Selector::RunDesc(desc.into()),
        desc: Some(desc.into()),
        expect: 10 * MINUTE,
        shell: Some(proof_of(pid)),
    })
}

fn attend_named(name: &str) -> Event {
    attend(Selector::Name(name.into()), None)
}

fn attend_reply(out: Output) -> AttendReply {
    match out {
        Output::Attend(reply) => reply,
        other => panic!("not an attend reply: {other:?}"),
    }
}

fn entry_on<'s>(st: &'s SessionState, task: &str) -> &'s Entry {
    st.attendances
        .iter()
        .find(|e| e.task() == Some(task))
        .unwrap_or_else(|| panic!("no entry on {task}: {:?}", st.attendances))
}

fn standing_by_task(st: &SessionState, f: &Facts<'_>) -> Vec<(Option<String>, Qualifies)> {
    st.standing(f)
        .into_iter()
        .map(|(e, q)| (e.task().map(str::to_owned), q))
        .collect()
}

/// A `clank run` entry on `run-1` and a subagent entry on the captured
/// subagent, both proven by one listing.
fn stacked(s: &SessionId) -> SessionState {
    let mut st = SessionState::new(s.clone());
    st.step(attend_run("test run", 7001), &facts(T0, s, &[]));
    st.step(attend_subagent(), &facts(T0, s, &[]));
    assert_eq!(
        verdict(st.step(
            listing(vec![
                run_task("run-1", "test run"),
                subagent(SUBAGENT_ID, SUBAGENT_NAME)
            ]),
            &facts(T0 + MINUTE, s, &[])
        )),
        Verdict::Park,
        "both covered, nothing owed"
    );
    st
}

#[test]
fn a_run_and_a_subagent_are_attended_side_by_side() {
    let s = session();
    let mut st = stacked(&s);
    let now = facts(T0 + MINUTE, &s, &[]);
    assert_eq!(
        standing_by_task(&st, &now),
        [
            (Some("run-1".into()), Qualifies::Yes),
            (Some(SUBAGENT_ID.into()), Qualifies::Yes)
        ]
    );
    let run_before = entry_on(&st, "run-1").clone();

    st.step(attend_subagent(), &facts(T0 + 2 * MINUTE, &s, &[]));
    assert_eq!(
        st.attendances.len(),
        3,
        "the request waits for a listing beside the entry it may renew"
    );
    assert_eq!(
        entry_on(&st, "run-1"),
        &run_before,
        "a later attend of the subagent leaves the run alone"
    );

    let v = verdict(st.step(
        listing(vec![run_task("run-1", "test run")]),
        &facts(T0 + 3 * MINUTE, &s, &[]),
    ));
    assert!(
        matches!(notices(&v), [Notice::NoMatch { .. }]),
        "the subagent ended, so the request finds nothing; the run still covers its own \
         task: {v:?}"
    );
    assert_eq!(
        standing_by_task(&st, &facts(T0 + 3 * MINUTE, &s, &[])),
        [(Some("run-1".into()), Qualifies::Yes)]
    );
    assert_eq!(entry_on(&st, "run-1"), &run_before);
}

#[test]
fn a_name_that_matches_only_shell_work_is_refused_and_touches_nothing_else() {
    let s = session();
    let mut st = stacked(&s);
    let before = st.attendances.clone();
    st.step(
        attend_named("Run the full integration test harness"),
        &facts(T0 + 2 * MINUTE, &s, &[]),
    );
    let v = verdict(st.step(
        listing(vec![
            run_task("run-1", "test run"),
            subagent(SUBAGENT_ID, SUBAGENT_NAME),
        ]),
        &facts(T0 + 3 * MINUTE, &s, &[]),
    ));
    assert_eq!(
        notices(&v),
        [Notice::ShellNamed {
            name: "Run the full integration test harness".into(),
            task_id: "run-1".into()
        }]
    );
    assert_eq!(st.attendances, before);
}

#[test]
fn live_work_nothing_covers_wakes_the_agent_and_covered_work_parks() {
    let s = session();
    let shell: BackgroundTask = serde_json::from_value(serde_json::json!({
        "id": "bash-1", "type": "shell", "status": "running",
        "description": "Run the full integration test harness",
        "command": "cargo test",
    }))
    .unwrap();
    let mut st = SessionState::new(s.clone());
    let v = verdict(st.step(listing(vec![shell]), &facts(T0, &s, &[])));
    assert_eq!(
        notices(&v),
        [Notice::Uncovered(vec![Uncovered {
            task_id: "bash-1".into(),
            work: Attendable::Shell { run: None },
            entry: None,
        }])]
    );
    assert!(
        matches!(&v, Verdict::Deliver { identities, .. } if identities == &["uncovered:bash-1"]),
        "{v:?}"
    );

    let mut covered = SessionState::new(s.clone());
    covered.step(attend_run("test run", 7001), &facts(T0, &s, &[]));
    assert_eq!(
        verdict(covered.step(
            listing(vec![run_task("run-1", "test run")]),
            &facts(T0, &s, &[])
        )),
        Verdict::Park
    );

    let mut idle = SessionState::new(s.clone());
    assert_eq!(
        verdict(idle.step(listing(Vec::new()), &facts(T0, &s, &[]))),
        Verdict::Park
    );
}

#[test]
fn only_the_uncovered_task_is_named_and_the_receipt_mask_stays_global() {
    let s = session();
    let a = owed("event:a", "read:a");
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let mut st = received_then_attending(&s, &a);
    let both = || {
        listing(vec![
            subagent(SUBAGENT_ID, SUBAGENT_NAME),
            subagent("b-task", "other helper"),
        ])
    };
    let v = verdict(st.step(both(), &facts(T0 + 2 * MINUTE, &s, &a)));
    assert_eq!(uncovered(&v), ["b-task"], "names B only");
    assert!(
        matches!(&v, Verdict::Deliver { identities, .. } if identities == &["uncovered:b-task"]),
        "received work stays quiet: {v:?}"
    );
    assert_eq!(
        notices(&v),
        [Notice::Uncovered(vec![Uncovered {
            task_id: "b-task".into(),
            work: Attendable::Subagent {
                name: "other helper".into()
            },
            entry: None,
        }])]
    );

    let (shown, _) = presented(v);
    committed(st.step(Event::Delivered(shown), &facts(T0 + 2 * MINUTE, &s, &a)));
    assert_eq!(
        st.received,
        ["event:a"],
        "a demand is counted, never received"
    );

    let v = verdict(st.step(both(), &facts(T0 + 3 * MINUTE, &s, &ab)));
    let Verdict::Deliver { identities, .. } = &v else {
        panic!("new work still wakes: {v:?}");
    };
    assert_eq!(identities, &["event:b", "uncovered:b-task"]);
    assert_eq!(uncovered(&v), ["b-task"]);
}

#[test]
fn work_no_permitted_command_could_cover_is_never_named() {
    let s = session();
    let wait: BackgroundTask = serde_json::from_value(serde_json::json!({
        "id": "w1", "type": "shell", "status": "running",
        "description": "clank wait", "command": "clank wait --author grok",
    }))
    .unwrap();
    let unnamed: BackgroundTask = serde_json::from_value(serde_json::json!({
        "type": "subagent", "status": "running", "description": "no id",
    }))
    .unwrap();
    for (case, tasks) in [
        ("a monitor task", tasks(MONITOR)),
        ("a clank wait", TaskSnapshot::Known(vec![wait])),
        ("a task with no id", TaskSnapshot::Known(vec![unnamed])),
        ("an unavailable listing", TaskSnapshot::Unavailable),
    ] {
        let mut st = SessionState::new(s.clone());
        assert_eq!(
            verdict(st.step(
                Event::TurnEnd(report(Tool::Claude, tasks)),
                &facts(T0, &s, &[])
            )),
            Verdict::Park,
            "{case}"
        );
    }
    for tool in [Tool::Codex, Tool::OpenCode] {
        let mut st = SessionState::new(s.clone());
        assert_eq!(
            verdict(st.step(
                Event::TurnEnd(report(tool, tasks(RUNNING))),
                &facts(T0, &s, &[])
            )),
            Verdict::Park,
            "{tool:?}"
        );
    }
}

#[test]
fn a_gated_stop_names_no_uncovered_work() {
    let s = session();
    let stop = || Event::TurnEnd(report(Tool::Claude, tasks(RUNNING)));
    let gated: [(&str, Facts<'_>, Verdict); 4] = [
        (
            "auto off",
            Facts {
                auto: Ok(AutoMode::Off),
                ..facts(T0, &s, &[])
            },
            Verdict::Silent(SilentWhy::AutoOff),
        ),
        (
            "asking",
            Facts {
                asking: true,
                ..facts(T0, &s, &[])
            },
            Verdict::Park,
        ),
        (
            "blocked",
            Facts {
                blocked: true,
                ..facts(T0, &s, &[])
            },
            Verdict::Park,
        ),
        (
            "a person's command waiting",
            Facts {
                user_command: true,
                ..facts(T0, &s, &[])
            },
            Verdict::Silent(SilentWhy::UserCommand),
        ),
    ];
    for (case, f, expected) in gated {
        let mut st = SessionState::new(s.clone());
        assert_eq!(verdict(st.step(stop(), &f)), expected, "{case}");
    }
    let mut st = SessionState::new(s.clone());
    let mut stale = report(Tool::Claude, tasks(RUNNING));
    stale.session = other_session();
    assert_eq!(
        verdict(st.step(Event::TurnEnd(stale), &facts(T0, &s, &[]))),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    assert_eq!(
        uncovered(&verdict(st.step(stop(), &facts(T0, &s, &[])))),
        [SUBAGENT_ID],
        "ungated, the same Stop names it"
    );
}

#[test]
fn a_failed_run_takes_back_only_its_own_entry() {
    let s = session();
    let mut st = stacked(&s);
    let before = st.attendances.clone();
    let deadlines = st.next_deadline();
    assert_eq!(
        attend_reply(st.step(
            attend_run("third run", 7003),
            &facts(T0 + 2 * MINUTE, &s, &[])
        )),
        AttendReply::Accepted
    );
    assert_eq!(st.attendances.len(), 3);
    assert_eq!(
        attend_reply(st.step(
            Event::Attend(AttendRequest::Clear {
                session: s.clone(),
                which: Clearing::Run(proof_of(7003)),
            }),
            &facts(T0 + 2 * MINUTE, &s, &[])
        )),
        AttendReply::Cleared { removed: 1 }
    );
    assert_eq!(st.attendances, before, "deadlines and check-ins intact");
    assert_eq!(st.next_deadline(), deadlines);
}

#[test]
fn re_attending_a_subagent_renews_its_entry_and_no_other() {
    let s = session();
    let mut st = stacked(&s);
    let run_before = entry_on(&st, "run-1").clone();
    assert_eq!(
        attend_reply(st.step(attend_subagent(), &facts(T0 + 5 * MINUTE, &s, &[]))),
        AttendReply::Accepted
    );
    st.step(
        listing(vec![
            run_task("run-1", "test run"),
            subagent(SUBAGENT_ID, SUBAGENT_NAME),
        ]),
        &facts(T0 + 6 * MINUTE, &s, &[]),
    );
    assert_eq!(st.attendances.len(), 2, "renewed in place, never doubled");
    assert_eq!(entry_on(&st, SUBAGENT_ID).deadline(), T0 + 15 * MINUTE);
    assert_eq!(entry_on(&st, SUBAGENT_ID).check_in, CheckIn::Armed);
    assert_eq!(entry_on(&st, "run-1"), &run_before);
}

#[test]
fn a_second_run_with_a_desc_in_use_is_refused_and_the_first_still_resolves() {
    let s = session();
    for resolved_first in [false, true] {
        let mut st = SessionState::new(s.clone());
        st.step(attend_run("test run", 7001), &facts(T0, &s, &[]));
        let only_a = || listing(vec![run_task("run-a", "test run")]);
        if resolved_first {
            st.step(only_a(), &facts(T0, &s, &[]));
        }
        assert_eq!(
            attend_reply(st.step(attend_run("test run", 7002), &facts(T0 + 1, &s, &[]))),
            AttendReply::DescInUse("test run".into()),
            "resolved first: {resolved_first}"
        );
        assert_eq!(st.attendances.len(), 1);
        assert_eq!(
            verdict(st.step(only_a(), &facts(T0 + MINUTE, &s, &[]))),
            Verdict::Park
        );
        let a = entry(&st);
        assert_eq!(
            (a.task(), a.deadline(), &a.check_in),
            (Some("run-a"), T0 + 10 * MINUTE, &CheckIn::Armed),
            "resolved first: {resolved_first}"
        );

        st.step(listing(Vec::new()), &facts(T0 + 2 * MINUTE, &s, &[]));
        assert!(st.attendances.is_empty());
        assert_eq!(
            attend_reply(st.step(
                attend_run("test run", 7002),
                &facts(T0 + 3 * MINUTE, &s, &[])
            )),
            AttendReply::Accepted,
            "once A's entry has ended its desc is free"
        );
    }
}

#[test]
fn clearing_a_name_both_a_subagent_and_a_run_carry_is_refused() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(attend_named("probe"), &facts(T0, &s, &[]));
    st.step(attend_run("probe", 7001), &facts(T0, &s, &[]));
    let before = st.attendances.clone();
    let clear = |name: &str| {
        Event::Attend(AttendRequest::Clear {
            session: session(),
            which: Clearing::Named(name.into()),
        })
    };
    assert_eq!(
        attend_reply(st.step(clear("probe"), &facts(T0, &s, &[]))),
        AttendReply::Ambiguous(vec![
            Selector::Name("probe".into()),
            Selector::RunDesc("probe".into()),
        ])
    );
    assert_eq!(st.attendances, before);

    let mut one = SessionState::new(s.clone());
    one.step(attend_run("probe", 7001), &facts(T0, &s, &[]));
    one.step(attend_named("other"), &facts(T0, &s, &[]));
    assert_eq!(
        attend_reply(one.step(clear("probe"), &facts(T0, &s, &[]))),
        AttendReply::Cleared { removed: 1 }
    );
    assert_eq!(
        one.attendances
            .iter()
            .map(|e| e.selector.clone())
            .collect::<Vec<_>>(),
        [Selector::Name("other".into())]
    );
}

#[test]
fn entries_due_together_share_one_check_in_and_a_stale_result_is_discarded() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(attend_run("test run", 7001), &facts(T0, &s, &[]));
    st.step(attend_subagent(), &facts(T0, &s, &[]));
    st.step(attend_named("later helper"), &facts(T0 + MINUTE, &s, &[]));
    st.step(
        listing(vec![
            run_task("run-1", "test run"),
            subagent(SUBAGENT_ID, SUBAGENT_NAME),
            subagent("later", "later helper"),
        ]),
        &facts(T0 + 2 * MINUTE, &s, &[]),
    );
    assert_eq!(st.next_deadline(), Some(T0 + 10 * MINUTE), "the nearest");

    let fired = check_ins(effects(
        st.step(Event::CheckInDue, &facts(T0 + 10 * MINUTE, &s, &[])),
    ));
    assert_eq!(
        fired,
        [
            CheckInRef {
                key: EntryKey::Run(proof_of(7001)),
                since: T0
            },
            CheckInRef {
                key: EntryKey::Task(SUBAGENT_ID.into()),
                since: T0
            },
        ],
        "one check-in for both, not the one not yet due"
    );
    assert_eq!(entry_on(&st, "later").check_in, CheckIn::Armed);
    assert_eq!(st.next_deadline(), Some(T0 + 11 * MINUTE));

    st.step(attend_subagent(), &facts(T0 + 10 * MINUTE + 1, &s, &[]));
    st.step(
        listing(vec![
            run_task("run-1", "test run"),
            subagent(SUBAGENT_ID, SUBAGENT_NAME),
            subagent("later", "later helper"),
        ]),
        &facts(T0 + 10 * MINUTE + 1, &s, &[]),
    );
    st.step(
        Event::CheckInUndeliverable {
            check_ins: fired,
            why: "typing it did not start".into(),
        },
        &facts(T0 + 10 * MINUTE + 2, &s, &[]),
    );
    assert_eq!(
        entry_on(&st, SUBAGENT_ID).check_in,
        CheckIn::Armed,
        "renewed meanwhile: the old result is discarded"
    );
    assert_eq!(
        entry_on(&st, "run-1").check_in,
        CheckIn::Undeliverable("typing it did not start".into())
    );
    assert_eq!(entry_on(&st, "later").check_in, CheckIn::Armed);
}

#[test]
fn a_run_whose_process_is_gone_stays_visible_until_its_task_leaves() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(attend_run("test run", 7001), &facts(T0, &s, &[]));
    let listed = || listing(vec![run_task("run-1", "test run")]);
    st.step(listed(), &facts(T0, &s, &[]));
    let dead = Facts {
        liveness: &DEAD,
        ..facts(T0 + MINUTE, &s, &[])
    };
    let v = verdict(st.step(listed(), &dead));
    assert_eq!(
        notices(&v),
        [Notice::Uncovered(vec![Uncovered {
            task_id: "run-1".into(),
            work: Attendable::Shell {
                run: Some("test run".into())
            },
            entry: Some(Unqualified::ProcessGone),
        }])]
    );
    assert_eq!(
        standing_by_task(&st, &dead),
        [(
            Some("run-1".into()),
            Qualifies::No(Unqualified::ProcessGone)
        )]
    );
    assert!(
        effects(st.step(
            Event::CheckInDue,
            &Facts {
                liveness: &DEAD,
                ..facts(T0 + 10 * MINUTE, &s, &[])
            }
        ))
        .is_empty()
    );
    assert_eq!(entry(&st).check_in, CheckIn::ProcessGone);
    st.step(listing(Vec::new()), &dead);
    assert!(st.attendances.is_empty());
}

#[test]
fn a_run_past_its_expectation_is_renewed_by_its_desc() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(attend_run("test run", 7001), &facts(T0, &s, &[]));
    st.step(
        listing(vec![run_task("run-1", "test run")]),
        &facts(T0, &s, &[]),
    );
    fired(&mut st, T0 + 10 * MINUTE);
    let run_before = st.attendances.clone();
    assert_eq!(
        attend_reply(st.step(attend_named("test run"), &facts(T0 + 11 * MINUTE, &s, &[]))),
        AttendReply::Accepted,
        "a name means the run only once a listing shows it is the run"
    );
    assert_eq!(st.attendances[0], run_before[0]);
    assert_eq!(
        verdict(st.step(
            listing(vec![run_task("run-1", "test run")]),
            &facts(T0 + 12 * MINUTE, &s, &[])
        )),
        Verdict::Park
    );
    assert_eq!(st.attendances.len(), 1);
    assert_eq!(entry(&st).deadline(), T0 + 21 * MINUTE);
    assert_eq!(entry(&st).shell, Some(proof_of(7001)));
}

/// `turn.json` as monitors before attendance sets wrote it, one per shape.
mod old_bytes {
    pub const NONE: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":"None","repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":[],"unwatched":null}"#;
    pub const INTENT: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":{"Intent":{"selector":{"RunDesc":"test run"},"desc":"test run","since":1000000,"expect":600000,"shell":{"pid":7001,"token":"mac1:7001:1"}}},"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":[],"unwatched":null}"#;
    pub const STANDING: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":{"Standing":{"task_id":"a25eee2eef81caf3e","kind":"Subagent","desc":"capture-probe sleeper","since":1000000,"expect":600000,"shell":null,"check_in":"Armed","renewal":null}},"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
    pub const SAME_RENEWAL: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":{"Standing":{"task_id":"a25eee2eef81caf3e","kind":"Subagent","desc":"capture-probe sleeper","since":1000000,"expect":600000,"shell":null,"check_in":"Armed","renewal":{"selector":{"Name":"capture-probe sleeper"},"desc":"capture-probe sleeper","since":1120000,"expect":1200000,"shell":null}}},"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
    pub const OTHER_RENEWAL: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":{"Standing":{"task_id":"a25eee2eef81caf3e","kind":"Subagent","desc":"capture-probe sleeper","since":1000000,"expect":600000,"shell":null,"check_in":"Armed","renewal":{"selector":{"RunDesc":"test run"},"desc":"test run","since":1120000,"expect":1200000,"shell":{"pid":7001,"token":"mac1:7001:1"}}}},"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
}

fn restore(bytes: &str) -> SessionState {
    SessionState::restored(serde_json::from_str(bytes).expect("old bytes load"))
}

#[test]
fn every_old_single_attendance_shape_loads_as_a_set() {
    let s = session();
    let sleeper = || Entry {
        selector: Selector::Name(SUBAGENT_NAME.into()),
        resolved: Some(Resolved {
            task_id: SUBAGENT_ID.into(),
            kind: TaskKind::Subagent,
        }),
        desc: Some(SUBAGENT_NAME.into()),
        since: T0,
        expect: 10 * MINUTE,
        shell: None,
        check_in: CheckIn::Armed,
    };
    let run_request = || Entry {
        selector: Selector::RunDesc("test run".into()),
        resolved: None,
        desc: Some("test run".into()),
        since: T0,
        expect: 10 * MINUTE,
        shell: Some(proof_of(7001)),
        check_in: CheckIn::Unarmed,
    };
    assert!(restore(old_bytes::NONE).attendances.is_empty());
    assert_eq!(restore(old_bytes::NONE).next_seq, 3);
    assert_eq!(restore(old_bytes::INTENT).attendances, [run_request()]);
    assert_eq!(restore(old_bytes::STANDING).attendances, [sleeper()]);
    assert_eq!(
        restore(old_bytes::SAME_RENEWAL).attendances,
        [
            sleeper(),
            Entry {
                resolved: None,
                since: T0 + 2 * MINUTE,
                expect: 20 * MINUTE,
                check_in: CheckIn::Unarmed,
                ..sleeper()
            }
        ],
        "the named renewal waits for a listing to say which task it means"
    );
    assert_eq!(
        restore(old_bytes::OTHER_RENEWAL).attendances,
        [
            sleeper(),
            Entry {
                since: T0 + 2 * MINUTE,
                expect: 20 * MINUTE,
                ..run_request()
            }
        ],
        "the standing entry and the request for other work"
    );

    for (bytes, with_run) in [
        (old_bytes::STANDING, false),
        (old_bytes::SAME_RENEWAL, false),
        (old_bytes::OTHER_RENEWAL, true),
    ] {
        let a = owed("event:a", "read:a");
        let mut st = restore(bytes);
        assert_eq!(
            st.recovery_quiet(&facts(T0 + 3 * MINUTE, &s, &a)),
            None,
            "unproven until a fresh listing"
        );
        assert_eq!(
            verdict(
                st.step(
                    listing(
                        std::iter::once(subagent(SUBAGENT_ID, SUBAGENT_NAME))
                            .chain(with_run.then(|| run_task("run-1", "test run")))
                            .collect()
                    ),
                    &facts(T0 + 3 * MINUTE, &s, &a)
                )
            ),
            Verdict::Park,
            "a fresh listing proves it and it keeps quieting what was received"
        );
        assert_eq!(st.received, ["event:a"]);
        assert_eq!(
            st.recovery_quiet(&facts(T0 + 3 * MINUTE, &s, &a)),
            Some(Quiet::Attending)
        );
        if bytes == old_bytes::SAME_RENEWAL {
            assert_eq!(
                st.attendances,
                [Entry {
                    since: T0 + 2 * MINUTE,
                    expect: 20 * MINUTE,
                    ..sleeper()
                }],
                "the listing shows the same task: renewed in place"
            );
        }
    }

    let mut replaced = restore(old_bytes::SAME_RENEWAL);
    assert_eq!(
        verdict(replaced.step(
            listing(vec![subagent("b-task", SUBAGENT_NAME)]),
            &facts(T0 + 3 * MINUTE, &s, &[])
        )),
        Verdict::Park,
        "the replacement is attended, so nothing names it as uncovered"
    );
    let b = entry(&replaced);
    assert_eq!(
        (b.task(), b.since, b.expect),
        (Some("b-task"), T0 + 2 * MINUTE, 20 * MINUTE),
        "the name now means the replacement, under the renewal's expectation"
    );

    let back: Persisted = serde_json::from_str(
        &serde_json::to_string(&restore(old_bytes::OTHER_RENEWAL).persisted()).unwrap(),
    )
    .unwrap();
    assert_eq!(back.attendances.len(), 2, "the new shape round-trips");
}

#[test]
fn a_subagent_named_like_a_runs_desc_is_attended_and_the_run_is_left_alone() {
    let s = session();
    for run_first in [true, false] {
        let mut st = SessionState::new(s.clone());
        let mut replies = Vec::new();
        let steps: [(Event, Ms); 2] = [
            (attend_run(SUBAGENT_NAME, 7001), T0),
            (attend_subagent(), T0 + MINUTE),
        ];
        let order: Vec<&(Event, Ms)> = if run_first {
            steps.iter().collect()
        } else {
            steps.iter().rev().collect()
        };
        for (event, at) in order {
            replies.push(attend_reply(st.step(event.clone(), &facts(*at, &s, &[]))));
        }
        assert_eq!(
            replies,
            [AttendReply::Accepted, AttendReply::Accepted],
            "run first: {run_first}"
        );
        assert_eq!(
            verdict(st.step(
                listing(vec![
                    run_task("run-1", SUBAGENT_NAME),
                    subagent(SUBAGENT_ID, SUBAGENT_NAME),
                ]),
                &facts(T0 + 2 * MINUTE, &s, &[])
            )),
            Verdict::Park,
            "run first: {run_first}"
        );
        let run = entry_on(&st, "run-1");
        assert_eq!(
            (run.since, &run.shell, &run.check_in),
            (T0, &Some(proof_of(7001)), &CheckIn::Armed),
            "run first: {run_first}"
        );
        assert_eq!(entry_on(&st, SUBAGENT_ID).since, T0 + MINUTE);
        assert_eq!(st.attendances.len(), 2);
    }
}

#[test]
fn a_subagent_no_name_alone_resolves_is_never_demanded() {
    let s = session();
    let nameless: BackgroundTask = serde_json::from_value(serde_json::json!({
        "id": "n1", "type": "subagent", "status": "running",
    }))
    .unwrap();
    for (case, live) in [
        ("absent", vec![nameless]),
        ("blank", vec![subagent("n2", "")]),
        ("padded", vec![subagent("n3", " helper ")]),
        (
            "shared",
            vec![subagent("t1", "twin"), subagent("t2", "twin")],
        ),
    ] {
        let mut st = SessionState::new(s.clone());
        assert_eq!(
            verdict(st.step(listing(live), &facts(T0, &s, &[]))),
            Verdict::Park,
            "{case}"
        );
    }
    let mut st = SessionState::new(s.clone());
    let v = verdict(st.step(
        listing(vec![
            subagent("t1", "twin"),
            subagent("t2", "twin"),
            subagent("u1", "only one"),
        ]),
        &facts(T0, &s, &[]),
    ));
    assert_eq!(uncovered(&v), ["u1"], "a name that resolves is demanded");
}

/// An uncovered task's demand is a counted presentation: delivered again
/// and again it escalates, a failed delivery commits no count, and the
/// task covered or changed ends the episode.
#[test]
fn an_uncovered_demand_is_counted_like_owed_work() {
    let s = session();
    let stop = |position: u64, live: Vec<BackgroundTask>| {
        let mut r = report(Tool::Claude, TaskSnapshot::Known(live));
        r.place = TurnPlace {
            turn: Some(format!("turn-{position}")),
            position: Some(position),
            human_turn: false,
        };
        Event::TurnEnd(r)
    };
    let sleeper = || vec![subagent(SUBAGENT_ID, SUBAGENT_NAME)];
    let deliver = |st: &mut SessionState, v: Verdict, ok: bool| -> (u32, bool) {
        let Verdict::Deliver {
            presentation,
            repeat,
            escalated,
            identities,
            ..
        } = v
        else {
            panic!("{v:?}");
        };
        assert_eq!(identities.len(), 1, "{identities:?}");
        let result = if ok {
            Event::Delivered(presentation)
        } else {
            Event::EmitFailed(presentation)
        };
        settled(st.step(result, &facts(T0, &s, &[])));
        (repeat, escalated)
    };

    let mut st = SessionState::new(s.clone());
    let mut counts = Vec::new();
    for position in 1..=ESCALATE_AT as u64 {
        let v = verdict(st.step(stop(position, sleeper()), &facts(T0, &s, &[])));
        counts.push(deliver(&mut st, v, true));
    }
    assert_eq!(counts, [(1, false), (2, false), (3, false), (4, true)]);
    assert!(st.received.is_empty(), "counted, never received");

    let v = verdict(st.step(stop(5, sleeper()), &facts(T0, &s, &[])));
    assert_eq!(deliver(&mut st, v, false), (5, true));
    let v = verdict(st.step(stop(6, sleeper()), &facts(T0, &s, &[])));
    assert_eq!(
        deliver(&mut st, v, true),
        (5, true),
        "the failed delivery counted nothing"
    );

    st.step(attend_subagent(), &facts(T0, &s, &[]));
    assert_eq!(
        verdict(st.step(stop(7, sleeper()), &facts(T0, &s, &[]))),
        Verdict::Park
    );
    assert_eq!(st.repeat, None, "covered: the episode is over");
    st.step(clear_all(), &facts(T0, &s, &[]));
    let v = verdict(st.step(stop(8, sleeper()), &facts(T0, &s, &[])));
    assert_eq!(deliver(&mut st, v, true), (1, false));

    let v = verdict(st.step(
        stop(9, vec![subagent("other", "other helper")]),
        &facts(T0, &s, &[]),
    ));
    assert_eq!(
        deliver(&mut st, v, true),
        (1, false),
        "another task is another episode"
    );
}

#[test]
fn the_last_look_offers_only_the_entries_still_owed_their_check_in() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(attend_subagent(), &facts(T0, &s, &[]));
    st.step(attend_named("other helper"), &facts(T0, &s, &[]));
    st.step(
        listing(vec![
            subagent(SUBAGENT_ID, SUBAGENT_NAME),
            subagent("other", "other helper"),
        ]),
        &facts(T0, &s, &[]),
    );
    let due = T0 + 10 * MINUTE;
    let fired = check_ins(effects(st.step(Event::CheckInDue, &facts(due, &s, &[]))));
    assert_eq!(fired.len(), 2);
    st.step(attend_subagent(), &facts(due + 1, &s, &[]));
    st.step(
        listing(vec![
            subagent(SUBAGENT_ID, SUBAGENT_NAME),
            subagent("other", "other helper"),
        ]),
        &facts(due + 1, &s, &[]),
    );
    let lines = st
        .may_type_check_in(&fired, &facts(due + 2, &s, &[]))
        .unwrap();
    assert_eq!(
        lines.iter().map(|l| l.task_id.as_str()).collect::<Vec<_>>(),
        ["other"],
        "the renewed one drops out, the other is still owed"
    );
    st.step(clear_all(), &facts(due + 3, &s, &[]));
    assert!(
        st.may_type_check_in(&fired, &facts(due + 3, &s, &[]))
            .is_err()
    );
}

mod legacy_task_bytes {
    pub const INTENT: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":{"Intent":{"selector":{"TaskId":"legacy-shell"},"desc":"test run","since":1000000,"expect":600000,"shell":{"pid":7001,"token":"mac1:7001:1"}}},"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
    pub const RENEWAL: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":{"Standing":{"task_id":"a25eee2eef81caf3e","kind":"Subagent","desc":"capture-probe sleeper","since":1000000,"expect":600000,"shell":null,"check_in":"Armed","renewal":{"selector":{"TaskId":"legacy-shell"},"desc":"test run","since":1120000,"expect":1200000,"shell":{"pid":7001,"token":"mac1:7001:1"}}}},"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
}

#[test]
fn a_legacy_task_id_intent_or_renewal_is_kept_and_proven_only_by_fresh_evidence() {
    let s = session();
    let a = owed("event:a", "read:a");
    let legacy = || shell_task("legacy-shell");

    let mut st = restore(legacy_task_bytes::INTENT);
    assert_eq!(
        st.attendances
            .iter()
            .map(|e| (&e.selector, &e.shell))
            .collect::<Vec<_>>(),
        [(
            &Selector::TaskId("legacy-shell".into()),
            &Some(proof_of(7001))
        )]
    );
    assert_eq!(st.recovery_quiet(&facts(T0 + MINUTE, &s, &a)), None);
    let dead = Facts {
        liveness: &DEAD,
        ..facts(T0 + MINUTE, &s, &a)
    };
    let mut gone = st.clone();
    gone.step(listing(vec![legacy()]), &dead);
    assert_eq!(
        standing_by_task(&gone, &dead),
        [(
            Some("legacy-shell".into()),
            Qualifies::No(Unqualified::ProcessGone)
        )],
        "the listing alone is not its process"
    );
    assert_eq!(
        verdict(st.step(listing(vec![legacy()]), &facts(T0 + MINUTE, &s, &a))),
        Verdict::Park
    );
    assert_eq!(
        st.recovery_quiet(&facts(T0 + MINUTE, &s, &a)),
        Some(Quiet::Attending)
    );

    let mut st = restore(legacy_task_bytes::RENEWAL);
    assert_eq!(st.attendances.len(), 2);
    assert_eq!(
        st.attendances[1].selector,
        Selector::TaskId("legacy-shell".into())
    );
    assert_eq!(
        verdict(st.step(
            listing(vec![subagent(SUBAGENT_ID, SUBAGENT_NAME), legacy()]),
            &facts(T0 + 3 * MINUTE, &s, &a)
        )),
        Verdict::Park
    );
    assert_eq!(
        standing_by_task(&st, &facts(T0 + 3 * MINUTE, &s, &a)),
        [
            (Some(SUBAGENT_ID.into()), Qualifies::Yes),
            (Some("legacy-shell".into()), Qualifies::Yes)
        ]
    );
}

/// A name whose task ended and passed to a new task before the next
/// listing: the request is kept apart from the old entry until the listing
/// shows which task the name means, then attends the new one. The old
/// entry is retired with its task and nothing else is touched.
#[test]
fn a_name_passed_to_a_new_task_attends_the_new_task_and_retires_the_old() {
    let s = session();
    let mut st = stacked(&s);
    let run_before = entry_on(&st, "run-1").clone();
    assert_eq!(
        attend_reply(st.step(attend_subagent(), &facts(T0 + 5 * MINUTE, &s, &[]))),
        AttendReply::Accepted
    );
    assert_eq!(
        entry_on(&st, SUBAGENT_ID).deadline(),
        T0 + 10 * MINUTE,
        "the old entry is not renewed on the name alone"
    );
    let now = facts(T0 + 6 * MINUTE, &s, &[]);
    let v = verdict(st.step(
        listing(vec![
            run_task("run-1", "test run"),
            subagent("b-task", SUBAGENT_NAME),
        ]),
        &now,
    ));
    assert_eq!(v, Verdict::Park);
    assert!(uncovered(&v).is_empty(), "{v:?}");
    assert_eq!(
        standing_by_task(&st, &now),
        [
            (Some("run-1".into()), Qualifies::Yes),
            (Some("b-task".into()), Qualifies::Yes)
        ],
        "the old task's entry is retired"
    );
    assert_eq!(entry_on(&st, "b-task").deadline(), T0 + 15 * MINUTE);
    assert_eq!(entry_on(&st, "run-1"), &run_before);
}
