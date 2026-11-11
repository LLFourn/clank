//! A declared subagent through the work it may have started: the
//! transition table, and the sequence recorded on 2026-11-11 under
//! `tests/fixtures/stop_hook/`.

use super::*;

const A: &str = "a2185a1add00531a0";
const B: &str = "aeedcce3823eb5bd2";
const SHELL: &str = "bl2req1k0";
const MONITOR: &str = "sxc82ytp5";
const A_NAME: &str = "probe-A background sleeper";

const X: &str = "x1";
const X_NAME: &str = "Implement X";
const Y: &str = "y1";
const Y_NAME: &str = "Implement Y";

/// Hook times are given from here: later than any [`later`] hands out.
const H: Ms = 2_000_000_000;

fn recorded(fixture: &str) -> HookInput {
    let path = format!(
        "{}/tests/fixtures/stop_hook/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// One entry of the first recorded listing.
fn recorded_task(id: &str) -> BackgroundTask {
    recorded("subagent_stop_a_parks.json")
        .background_tasks
        .unwrap()
        .into_iter()
        .find(|t| t.id.as_deref() == Some(id))
        .unwrap()
}

fn in_turn(turn: &str, position: u64) -> TurnPlace {
    TurnPlace {
        turn: Some(turn.into()),
        position: Some(position),
        human_turn: false,
        read_to: None,
    }
}

fn agent_stop(tasks: Vec<BackgroundTask>, at: Ms, place: TurnPlace) -> TurnEndReport {
    TurnEndReport {
        tasks: TaskSnapshot::Known(tasks),
        hook_started: Some(at),
        place,
        ..report(Tool::Claude)
    }
}

fn listed_at(tasks: Vec<BackgroundTask>, at: Ms) -> Event {
    Event::TurnEnd(agent_stop(tasks, at, TurnPlace::default()))
}

fn recorded_stop(fixture: &str, at: Ms, place: TurnPlace) -> TurnEndReport {
    agent_stop(recorded(fixture).background_tasks.unwrap(), at, place)
}

fn child_stop(child: &str, tasks: Vec<BackgroundTask>, at: Option<Ms>) -> ChildReport {
    ChildReport {
        session: session(),
        child: child.into(),
        hook_started: at,
        seen: ChildSeen::Stopped {
            tasks: TaskSnapshot::Known(tasks),
            agent: TurnPlace::default(),
        },
    }
}

fn seeing_agent(mut report: ChildReport, record: TurnPlace) -> ChildReport {
    if let ChildSeen::Stopped { agent, .. } = &mut report.seen {
        *agent = record;
    }
    report
}

fn recorded_child_stop(fixture: &str, at: Ms) -> ChildReport {
    let input = recorded(fixture);
    child_stop(
        &input.agent_id.unwrap(),
        input.background_tasks.unwrap(),
        Some(at),
    )
}

fn child_start(child: &str, at: Option<Ms>) -> ChildReport {
    ChildReport {
        session: session(),
        child: child.into(),
        hook_started: at,
        seen: ChildSeen::Started,
    }
}

fn sub(id: &str, name: &str) -> BackgroundTask {
    BackgroundTask::from(serde_json::json!({
        "id": id, "type": "subagent", "status": "running", "description": name,
        "agent_type": "general-purpose"
    }))
}

fn shell(id: &str) -> BackgroundTask {
    BackgroundTask::from(serde_json::json!({
        "id": id, "type": "shell", "status": "running",
        "description": format!("run {id}"), "command": "sleep 240"
    }))
}

fn held(id: &str) -> HeldTask {
    HeldTask {
        id: id.into(),
        kind: Some("shell".into()),
        description: Some(format!("run {id}")),
    }
}

fn parked_on(ids: &[&str], sure: bool) -> Phase {
    Phase::Parked {
        on: ids.iter().map(|id| held(id)).collect(),
        sure,
    }
}

fn tell(st: &mut SessionState, report: ChildReport) {
    assert!(st.child(&report, &facts(T0, &session(), &[])), "{report:?}");
}

fn tracked(st: &SessionState) -> &Tracked {
    match &entry(st).work {
        Attending::Declared { tracked, .. } => tracked,
        other => panic!("not a declaration: {other:?}"),
    }
}

fn phase(st: &SessionState) -> Phase {
    tracked(st).phase.clone()
}

fn phase_of(st: &SessionState, task: &str) -> Phase {
    st.attendances
        .iter()
        .find(|e| e.task() == Some(task))
        .and_then(Entry::phase)
        .unwrap_or_else(|| panic!("`{task}` is not attended"))
        .clone()
}

/// What a monitor started now would hold: what was written, read back.
fn restarted(st: &SessionState) -> SessionState {
    let bytes = serde_json::to_string(&st.persisted()).unwrap();
    SessionState::restored(serde_json::from_str(&bytes).unwrap())
}

/// The report the agent is parked on, stepped again: what its waiter is
/// handed with no further Stop.
fn handed(st: &mut SessionState, owed: &[OwedItem]) -> Verdict {
    verdict(st.step(
        Event::Parked(TurnEndReport {
            hook_started: None,
            ..report(Tool::Claude)
        }),
        &facts(T0, &session(), owed),
    ))
}

/// `X` declared and bound by the agent's own Stop at `H + 1000`, which
/// listed it alone, with `a` received and so quiet.
fn bound_x(a: &[OwedItem]) -> SessionState {
    let s = session();
    let mut st = received_then(&s, a, declare(X_NAME));
    assert_eq!(
        verdict(st.step(listed_at(vec![sub(X, X_NAME)], H + 1000), &facts(T0, &s, a))),
        Verdict::Park
    );
    assert_eq!(phase(&st), Phase::Live);
    st
}

/// [`bound_x`], then `X` stopped at `H + 2000` with shell `s1` running.
fn parked_x(a: &[OwedItem]) -> SessionState {
    let mut st = bound_x(a);
    tell(
        &mut st,
        child_stop(X, vec![sub(X, X_NAME), shell("s1")], Some(H + 2000)),
    );
    assert_eq!(phase(&st), parked_on(&["s1"], false));
    assert_eq!(tracked(&st).as_of, Some(H + 2000));
    st
}

/// Every row of the plan's table, and what it leaves alone.
#[test]
fn the_transition_table() {
    use Order::{Later, Unordered};
    use Phase::{Ended, Live, Unsettled};
    let unsettled = || Unsettled { since: Some(9) };
    let unknown = || Unsettled { since: None };
    let unordered = Unordered { barrier: None };
    let stopped = |on: &[&str], sure| Sight::Stopped {
        candidates: on.iter().map(|id| held(id)).collect(),
        sure,
    };
    let absent = |agent_stop, live: &[&str]| Sight::Absent {
        agent_stop,
        live: live.iter().map(|id| id.to_string()).collect(),
        candidates: live.iter().map(|id| held(id)).collect(),
    };
    let parked = |sure| parked_on(&["s"], sure);
    let rows = [
        (
            "a child stop with work it may have started parks it",
            (Live, Later, stopped(&["s"], true)),
            parked(true),
        ),
        (
            "a child stop with none ends it",
            (Live, Later, stopped(&[], false)),
            Ended,
        ),
        (
            "gone from the agent's Stop with no child stop seen: parked, never sure",
            (Live, Later, absent(true, &["s"])),
            parked(false),
        ),
        (
            "gone from the agent's Stop with nothing newer listed: ended",
            (Live, Later, absent(true, &[])),
            Ended,
        ),
        (
            "another child's listing without a live one proves nothing",
            (Live, Later, absent(false, &[])),
            Live,
        ),
        (
            "a start resumes a parked one",
            (parked(true), Later, Sight::Started),
            Live,
        ),
        (
            "a listing showing a parked one resumes it",
            (parked(true), Later, Sight::Listed),
            Live,
        ),
        (
            "a start settles an unsettled one",
            (unsettled(), Later, Sight::Started),
            Live,
        ),
        (
            "a listing showing an unsettled one settles it",
            (unsettled(), Later, Sight::Listed),
            Live,
        ),
        (
            "parked, absent from a later listing with its work gone: ended",
            (parked(true), Later, absent(false, &["t"])),
            Ended,
        ),
        (
            "parked, absent from a later listing with its work live: parked",
            (parked(true), Later, absent(true, &["s", "t"])),
            parked(true),
        ),
        (
            "an unordered start unsettles a parked one",
            (parked(true), unordered, Sight::Started),
            unknown(),
        ),
        (
            "an unordered listing showing a parked one unsettles it",
            (parked(true), unordered, Sight::Listed),
            unknown(),
        ),
        (
            "unsettled, its own stop: as from live",
            (unsettled(), Later, stopped(&["s"], false)),
            parked(false),
        ),
        (
            "unsettled, its own stop with nothing: as from live",
            (unsettled(), Later, stopped(&[], false)),
            Ended,
        ),
        (
            "unsettled, gone from the agent's Stop: as from live",
            (unsettled(), Later, absent(true, &["s"])),
            parked(false),
        ),
        (
            "unsettled, gone from the agent's Stop with nothing: as from live",
            (unsettled(), Later, absent(true, &[])),
            Ended,
        ),
        (
            "unsettled, absent from another child's listing: stays",
            (unsettled(), Later, absent(false, &[])),
            unsettled(),
        ),
        (
            "unsettled, an unordered start: stays",
            (unsettled(), unordered, Sight::Started),
            unknown(),
        ),
        (
            "unsettled, an unordered stop: stays, its barrier kept",
            (unsettled(), unordered, stopped(&[], false)),
            unsettled(),
        ),
        (
            "unsettled, an unordered Stop of the agent's without it: stays, its barrier kept",
            (unsettled(), unordered, absent(true, &[])),
            unsettled(),
        ),
        (
            "an unordered stop cannot park or end a live one",
            (Live, unordered, stopped(&[], false)),
            Live,
        ),
        (
            "an unordered listing without a live one cannot end it",
            (Live, unordered, absent(true, &[])),
            Live,
        ),
        (
            "an unordered start leaves a live one live",
            (Live, unordered, Sight::Started),
            Live,
        ),
        (
            "an unordered stop cannot end a parked one",
            (parked(true), unordered, stopped(&[], false)),
            parked(true),
        ),
        (
            "an unordered listing without a parked one cannot end it",
            (parked(true), unordered, absent(true, &[])),
            parked(true),
        ),
        (
            "a parked one that stops again, its start unseen: as from live",
            (parked(true), Later, stopped(&["t"], false)),
            parked_on(&["t"], false),
        ),
        (
            "a parked one that stops again with nothing: ended",
            (parked(true), Later, stopped(&[], false)),
            Ended,
        ),
        (
            "an unordered start at the very time of the park keeps that time as the barrier",
            (parked(true), Unordered { barrier: Some(5) }, Sight::Started),
            Unsettled { since: Some(5) },
        ),
        (
            "unsettled, a report at its barrier keeps the barrier",
            (
                unsettled(),
                Unordered { barrier: Some(9) },
                stopped(&[], false),
            ),
            unsettled(),
        ),
        (
            "unsettled with no barrier: nothing ends it",
            (unknown(), unordered, stopped(&[], false)),
            unknown(),
        ),
        ("ended stays ended", (Ended, Later, Sight::Started), Ended),
    ];
    for (row, (from, order, sight), to) in rows {
        assert_eq!(transition(from, order, sight), to, "{row}");
    }
}

/// Only a hook time strictly after the last applied is later: equal,
/// earlier and missing are all unordered, and before anything is applied
/// any time is later.
#[test]
fn a_report_is_later_only_by_its_own_hooks_time() {
    for (report, as_of, order) in [
        (Some(5), Some(4), Order::Later),
        (Some(5), None, Order::Later),
        (Some(5), Some(5), Order::Unordered { barrier: Some(5) }),
        (Some(4), Some(5), Order::Unordered { barrier: None }),
        (None, Some(5), Order::Unordered { barrier: None }),
        (None, None, Order::Unordered { barrier: None }),
    ] {
        assert_eq!(
            Order::of(report, as_of),
            order,
            "{report:?} after {as_of:?}"
        );
    }
}

/// A session whose earlier Stop showed only its monitor, then `a`
/// received and A declared in the turn that delivery started.
fn declared_a(a: &[OwedItem]) -> SessionState {
    let s = session();
    let mut st = SessionState::new(s.clone());
    assert_eq!(
        verdict(st.step(
            listed_at(vec![recorded_task(MONITOR)], H),
            &facts(T0, &s, &[])
        )),
        Verdict::Park
    );
    let (p, _) = deliver(&mut st, &facts(T0, &s, a));
    committed(st.step(Event::Delivered(p), &facts(T0, &s, a)));
    st.step(declare(A_NAME), &facts(T0, &s, a));
    st
}

fn recorded_shell() -> HeldTask {
    HeldTask {
        id: SHELL.into(),
        kind: Some("shell".into()),
        description: Some("Sleep 240 seconds as a background timing probe".into()),
    }
}

/// The recorded sequence, after a Stop the recording does not hold (made
/// here from its own entries) that showed A running: A parks on its
/// shell, which the agent's open turn could also have started, so it is
/// held unsure; the agent's Stops that no longer list A end nothing; the
/// documented start for its resume makes it live; and its last stop,
/// which still lists A itself and no shell, ends the attendance and hands
/// the held work to the agent's parked waiter with no Stop of the agent's.
#[test]
fn the_recorded_sequence_holds_until_the_subagents_last_stop() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = declared_a(&a);
    let before = agent_stop(
        vec![recorded_task(MONITOR), recorded_task(A), recorded_task(B)],
        H + 10_000,
        in_turn("prompt-0", 10),
    );
    assert_eq!(
        verdict(st.step(Event::TurnEnd(before), &facts(T0, &s, &a))),
        Verdict::Park
    );
    assert_eq!(phase(&st), Phase::Live);

    let parked = Phase::Parked {
        on: vec![recorded_shell()],
        sure: false,
    };
    tell(
        &mut st,
        seeing_agent(
            recorded_child_stop("subagent_stop_a_parks.json", H + 46_100),
            in_turn("prompt-1", 90),
        ),
    );
    assert_eq!(phase(&st), parked, "the agent's turn was open");

    let stop = recorded_stop(
        "stop_a_parked_b_running.json",
        H + 46_150,
        in_turn("prompt-1", 90),
    );
    assert_eq!(
        verdict(st.step(Event::TurnEnd(stop), &facts(T0, &s, &a))),
        Verdict::Park,
        "A is absent from the agent's Stop and still attended"
    );
    tell(
        &mut st,
        recorded_child_stop("subagent_stop_b_finished.json", H + 49_000),
    );
    let stop = recorded_stop(
        "stop_a_parked_shell_running.json",
        H + 65_000,
        in_turn("prompt-2", 170),
    );
    assert_eq!(
        verdict(st.step(Event::TurnEnd(stop), &facts(T0, &s, &a))),
        Verdict::Park
    );
    assert_eq!(phase(&st), parked);

    tell(&mut st, child_start(A, Some(H + 287_000)));
    assert_eq!(phase(&st), Phase::Live);
    tell(
        &mut st,
        recorded_child_stop("subagent_stop_a_final.json", H + 288_000),
    );
    assert!(st.attendances.is_empty(), "its own entry is not presence");
    let (_, identities) = presented(handed(&mut st, &a));
    assert_eq!(identities, ["event:a"], "released to the parked waiter");
}

/// From a pending declaration, a start carrying only the documented
/// fields, and exactly the recorded payloads: no listing ever showed A
/// before its own first stop, which binds it. That listing's shell is a
/// candidate, not baseline; the monitor an earlier Stop showed is
/// baseline. Its resume's start is not sent here, and its last stop ends
/// the attendance from parked all the same.
#[test]
fn a_declaration_binds_from_the_subagents_own_first_stop() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = declared_a(&a);
    tell(&mut st, child_start(A, Some(H + 1000)));
    assert_eq!(bound_to(&st), [None], "a start names no description");
    assert_eq!(st.starts.get(A), Some(&Some(H + 1000)));

    tell(
        &mut st,
        recorded_child_stop("subagent_stop_a_parks.json", H + 46_100),
    );
    assert_eq!(bound_to(&st), [Some(A)]);
    assert_eq!(tracked(&st).baseline, [MONITOR]);
    let parked = Phase::Parked {
        on: vec![recorded_shell()],
        sure: false,
    };
    assert_eq!(phase(&st), parked);
    assert!(st.starts.is_empty(), "the start it was bound past");

    let agent_stops = |st: &mut SessionState, fixture: &str, at| {
        let stop = recorded_stop(fixture, at, TurnPlace::default());
        assert_eq!(
            verdict(st.step(Event::TurnEnd(stop), &facts(T0, &s, &a))),
            Verdict::Park,
            "{fixture}"
        );
        assert_eq!(phase(st), parked, "{fixture}");
        assert_eq!(tracked(st).as_of, Some(at), "{fixture}");
    };
    agent_stops(&mut st, "stop_a_parked_b_running.json", H + 46_150);
    tell(
        &mut st,
        recorded_child_stop("subagent_stop_b_finished.json", H + 49_000),
    );
    agent_stops(&mut st, "stop_a_parked_shell_running.json", H + 65_000);
    tell(
        &mut st,
        recorded_child_stop("subagent_stop_a_final.json", H + 288_000),
    );
    assert!(st.attendances.is_empty());
    let (_, identities) = presented(handed(&mut st, &a));
    assert_eq!(identities, ["event:a"]);
}

/// The two hooks of one second race. The agent's Stop, taken after A's
/// stop, can reach the monitor first; what it lists is then remembered
/// under its own later time, so the shell is still newer than A when A's
/// stop binds it.
#[test]
fn a_listing_that_arrives_first_but_was_taken_later_is_not_baseline() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = declared_a(&a);
    let stop = recorded_stop(
        "stop_a_parked_b_running.json",
        H + 46_150,
        TurnPlace::default(),
    );
    assert_eq!(
        verdict(st.step(Event::TurnEnd(stop), &facts(T0, &s, &a))),
        Verdict::Park
    );
    assert_eq!(st.seen.get(SHELL), Some(&Some(H + 46_150)));
    tell(
        &mut st,
        recorded_child_stop("subagent_stop_a_parks.json", H + 46_100),
    );
    assert_eq!(tracked(&st).baseline, [MONITOR]);
    assert_eq!(
        phase(&st),
        Phase::Parked {
            on: vec![recorded_shell()],
            sure: false,
        }
    );
    assert_eq!(
        st.seen.get(SHELL),
        Some(&Some(H + 46_100)),
        "the earliest time it was shown at"
    );

    let mut st = declared_a(&a);
    let stop = recorded_stop(
        "stop_a_parked_b_running.json",
        H + 46_100,
        TurnPlace::default(),
    );
    st.step(Event::TurnEnd(stop), &facts(T0, &s, &a));
    tell(
        &mut st,
        recorded_child_stop("subagent_stop_a_parks.json", H + 46_100),
    );
    assert_eq!(
        tracked(&st).baseline,
        [MONITOR],
        "shown at the very time A's stop was taken is not shown before it"
    );
}

/// A subagent that stops with nothing it could have started is finished:
/// bound already, or bound by that very stop.
#[test]
fn a_stop_with_no_candidates_ends_the_attendance_at_once() {
    let a = owed("event:a", "read:a");
    let mut st = bound_x(&a);
    tell(&mut st, child_stop(X, vec![sub(X, X_NAME)], Some(H + 2000)));
    assert!(st.attendances.is_empty());
    let (_, identities) = presented(handed(&mut st, &a));
    assert_eq!(identities, ["event:a"]);

    let mut st = received_then(&session(), &a, declare(X_NAME));
    tell(&mut st, child_stop(X, vec![sub(X, X_NAME)], Some(H + 2000)));
    assert!(st.attendances.is_empty(), "bound and ended by one stop");

    let untyped = BackgroundTask::from(serde_json::json!({
        "id": X, "status": "running", "description": X_NAME
    }));
    let mut st = bound_x(&a);
    tell(&mut st, child_stop(X, vec![untyped], Some(H + 2000)));
    assert!(
        st.attendances.is_empty(),
        "its own entry is never its work, whatever kind its stop's listing calls it"
    );
}

/// The agent's own `clank wait`, by any path and with any arguments, is
/// never a subagent's work, on its own stop or at the agent's Stop. A
/// command that only mentions one is a shell like any other.
#[test]
fn the_sessions_own_clank_wait_never_holds_a_subagent() {
    let s = session();
    let a = owed("event:a", "read:a");
    let task = |id: &str, command: &str| {
        BackgroundTask::from(serde_json::json!({
            "id": id, "type": "shell", "status": "running", "command": command
        }))
    };
    let waits = [
        "clank wait",
        "clank wait --for 30m",
        "/Users/me/.cargo/bin/clank wait --json",
        "\"/opt/clank dir/clank\" wait",
    ];
    for command in waits {
        let mut st = bound_x(&a);
        tell(
            &mut st,
            child_stop(X, vec![sub(X, X_NAME), task("w", command)], Some(H + 2000)),
        );
        assert!(st.attendances.is_empty(), "its own stop: {command}");

        let mut st = bound_x(&a);
        st.step(
            listed_at(vec![task("w", command)], H + 2000),
            &facts(T0, &s, &a),
        );
        assert!(st.attendances.is_empty(), "the agent's Stop: {command}");
    }
    for command in [
        "echo clank wait",
        "clank-wait",
        "clank waiter",
        "cargo test",
    ] {
        let mut st = bound_x(&a);
        tell(
            &mut st,
            child_stop(X, vec![sub(X, X_NAME), task("w", command)], Some(H + 2000)),
        );
        assert!(
            matches!(phase(&st), Phase::Parked { on, .. } if on.len() == 1 && on[0].id == "w"),
            "not a clank wait: {command}"
        );
    }
}

/// A task some listing showed before the subagent was first shown is not
/// its work: it never holds the attendance, by the subagent's own stop or
/// by the agent's.
#[test]
fn a_task_older_than_the_subagent_never_holds_it() {
    let s = session();
    let none: [OwedItem; 0] = [];
    let bound = || {
        let mut st = SessionState::new(s.clone());
        st.step(listed_at(vec![shell("old")], H), &facts(T0, &s, &none));
        st.step(declare(X_NAME), &facts(T0, &s, &none));
        st.step(
            listed_at(vec![shell("old"), sub(X, X_NAME)], H + 1000),
            &facts(T0, &s, &none),
        );
        assert_eq!(tracked(&st).baseline, ["old"]);
        st
    };
    let mut st = bound();
    tell(
        &mut st,
        child_stop(X, vec![shell("old"), sub(X, X_NAME)], Some(H + 2000)),
    );
    assert!(st.attendances.is_empty(), "its own stop");

    let mut st = bound();
    st.step(
        listed_at(vec![shell("old")], H + 2000),
        &facts(T0, &s, &none),
    );
    assert!(st.attendances.is_empty(), "the agent's Stop without it");
}

/// The agent's own shell, started in an open turn while the subagent ran,
/// cannot be told from the subagent's: first shown by the agent's Stop
/// that also first showed the subagent, or first shown after that Stop
/// once the agent has gone on, it holds the attendance and is never sure.
#[test]
fn the_agents_own_shell_holds_unsure() {
    let s = session();
    let a = owed("event:a", "read:a");
    let at_stop = in_turn("prompt-1", 40);
    let mut st = received_then(&s, &a, declare(X_NAME));
    st.step(
        Event::TurnEnd(agent_stop(
            vec![sub(X, X_NAME), shell("mine")],
            H + 1000,
            at_stop.clone(),
        )),
        &facts(T0, &s, &a),
    );
    assert!(tracked(&st).baseline.is_empty());
    tell(
        &mut st,
        seeing_agent(
            child_stop(X, vec![sub(X, X_NAME), shell("mine")], Some(H + 2000)),
            at_stop.clone(),
        ),
    );
    assert_eq!(
        phase(&st),
        parked_on(&["mine"], false),
        "the agent's Stop listed it"
    );

    let mut st = received_then(&s, &a, declare(X_NAME));
    st.step(
        Event::TurnEnd(agent_stop(vec![sub(X, X_NAME)], H + 1000, at_stop)),
        &facts(T0, &s, &a),
    );
    tell(
        &mut st,
        seeing_agent(
            child_stop(X, vec![sub(X, X_NAME), shell("mine")], Some(H + 2000)),
            in_turn("prompt-2", 90),
        ),
    );
    assert_eq!(
        phase(&st),
        parked_on(&["mine"], false),
        "the agent had gone on since its Stop"
    );
}

/// `sure` is what the two hooks saw, never where the agent is when the
/// report is applied. It holds only when the agent's Stop came first, the
/// agent's record was still that Stop's when the child's hook read it,
/// and the Stop's listing did not have the task.
#[test]
fn sure_only_when_the_agents_stop_still_stood_and_did_not_list_the_task() {
    let s = session();
    let a = owed("event:a", "read:a");
    let at_stop = in_turn("prompt-1", 40);
    let stop = |tasks, at| Event::TurnEnd(agent_stop(tasks, at, at_stop.clone()));
    let bound = |listed: Vec<BackgroundTask>| {
        let mut st = received_then(&s, &a, declare(X_NAME));
        st.step(stop(listed, H + 1000), &facts(T0, &s, &a));
        st
    };
    let stops = |record: TurnPlace| {
        seeing_agent(
            child_stop(X, vec![sub(X, X_NAME), shell("s1")], Some(H + 2000)),
            record,
        )
    };

    let mut st = bound(vec![sub(X, X_NAME)]);
    tell(&mut st, stops(at_stop.clone()));
    assert_eq!(phase(&st), parked_on(&["s1"], true));

    let mut st = bound(vec![sub(X, X_NAME)]);
    assert!(st.stopped.is_some(), "the monitor still holds its Stop");
    tell(&mut st, stops(in_turn("prompt-2", 90)));
    assert_eq!(
        phase(&st),
        parked_on(&["s1"], false),
        "the agent went on between the two"
    );

    let mut st = bound(vec![sub(X, X_NAME)]);
    let ab = [a[0].clone(), owed("message:b", "read:b")[0].clone()];
    let (p, _) = presented(handed(&mut st, &ab));
    committed(st.step(Event::Delivered(p), &facts(T0, &s, &ab)));
    tell(&mut st, stops(at_stop.clone()));
    assert_eq!(
        phase(&st),
        parked_on(&["s1"], false),
        "clank woke it past that Stop"
    );

    let mut st = bound(vec![sub(X, X_NAME), shell("s1")]);
    tell(&mut st, stops(at_stop.clone()));
    assert_eq!(
        phase(&st),
        parked_on(&["s1"], false),
        "its Stop listed the task"
    );

    let mut st = received_then(&s, &a, declare(X_NAME));
    st.step(stop(vec![], H + 3000), &facts(T0, &s, &a));
    tell(&mut st, stops(at_stop.clone()));
    assert_eq!(
        phase(&st),
        parked_on(&["s1"], false),
        "its Stop came after the child's"
    );

    let read = |len| TurnPlace {
        read_to: Some(ReadTo { file: 7, len }),
        ..TurnPlace::default()
    };
    for (len, sure) in [(100, true), (120, false)] {
        let mut st = received_then(&s, &a, declare(X_NAME));
        st.step(
            Event::TurnEnd(agent_stop(vec![sub(X, X_NAME)], H + 1000, read(100))),
            &facts(T0, &s, &a),
        );
        tell(&mut st, stops(read(len)));
        assert_eq!(
            phase(&st),
            parked_on(&["s1"], sure),
            "a Stop with no boundary, its record read to {len}"
        );
    }

    for (moved, why) in [
        (in_turn("prompt-1", 90), "the same turn, a later boundary"),
        (in_turn("prompt-2", 40), "another turn at the same offset"),
    ] {
        let mut st = bound(vec![sub(X, X_NAME)]);
        tell(&mut st, stops(moved));
        assert_eq!(phase(&st), parked_on(&["s1"], false), "{why}");
    }
    let unnamed = TurnPlace {
        turn: None,
        ..in_turn("", 40)
    };
    let mut st = received_then(&s, &a, declare(X_NAME));
    st.step(
        Event::TurnEnd(agent_stop(vec![sub(X, X_NAME)], H + 1000, unnamed.clone())),
        &facts(T0, &s, &a),
    );
    tell(&mut st, stops(unnamed));
    assert_eq!(
        phase(&st),
        parked_on(&["s1"], false),
        "a boundary of no named turn speaks for none"
    );
}

/// One shell, two declared subagents: it holds the one whose stop listed
/// it, and the other, still running, is untouched. Once it has gone, the
/// first is over whichever listing shows that.
#[test]
fn a_shell_is_a_candidate_of_the_child_whose_stop_listed_it() {
    let s = session();
    let none: [OwedItem; 0] = [];
    let mut st = SessionState::new(s.clone());
    st.step(declare(X_NAME), &facts(T0, &s, &none));
    st.step(declare(Y_NAME), &facts(T0, &s, &none));
    st.step(
        listed_at(vec![sub(X, X_NAME), sub(Y, Y_NAME)], H + 1000),
        &facts(T0, &s, &none),
    );
    tell(
        &mut st,
        child_stop(
            X,
            vec![sub(X, X_NAME), sub(Y, Y_NAME), shell("s1")],
            Some(H + 2000),
        ),
    );
    assert_eq!(phase_of(&st, X), parked_on(&["s1"], false));
    assert_eq!(phase_of(&st, Y), Phase::Live, "another subagent's stop");

    tell(
        &mut st,
        child_stop(Y, vec![sub(Y, Y_NAME), shell("s1")], Some(H + 3000)),
    );
    assert_eq!(phase_of(&st, X), parked_on(&["s1"], false));
    assert_eq!(phase_of(&st, Y), parked_on(&["s1"], false));

    st.step(listed_at(vec![], H + 4000), &facts(T0, &s, &none));
    assert!(st.attendances.is_empty());
}

/// A subagent that parks, resumes and parks again is attended throughout,
/// and its held work stays quiet until the stop that leaves it nothing.
#[test]
fn a_subagent_that_parks_twice_is_attended_throughout() {
    let a = owed("event:a", "read:a");
    let mut st = parked_x(&a);
    assert_eq!(handed(&mut st, &a), Verdict::Park);
    tell(&mut st, child_start(X, Some(H + 3000)));
    assert_eq!(phase(&st), Phase::Live);
    assert_eq!(handed(&mut st, &a), Verdict::Park);
    tell(
        &mut st,
        child_stop(X, vec![sub(X, X_NAME), shell("s2")], Some(H + 4000)),
    );
    assert_eq!(phase(&st), parked_on(&["s2"], false));
    assert_eq!(handed(&mut st, &a), Verdict::Park);
    tell(&mut st, child_start(X, Some(H + 5000)));
    tell(&mut st, child_stop(X, vec![sub(X, X_NAME)], Some(H + 6000)));
    assert!(st.attendances.is_empty());
    assert!(matches!(handed(&mut st, &a), Verdict::Deliver { .. }));
}

/// A start that cannot be ordered after the park might be a resume, so
/// the shell's disappearance stops proving anything, across a restart
/// too, and nothing is handed to the waiter. Its barrier is the time the
/// start shared with the park when the two are equal: a report strictly
/// after it then settles the subagent as from live. A start with no time,
/// or one earlier than the park, leaves none, since no later time proves
/// a report followed that resume: a listing showing the subagent running,
/// one where it has parked again unseen, the agent's own Stop without it
/// and its own stop with nothing left all keep it unsettled. An ordered
/// start makes it live, and its later stop with nothing left ends it.
#[test]
fn an_unordered_start_keeps_a_parked_subagent_attended() {
    let s = session();
    let a = owed("event:a", "read:a");
    let other = || sub(B, "another subagent");
    for (at, since) in [
        (Some(H + 2000), Some(H + 2000)),
        (None, None),
        (Some(H + 1500), None),
    ] {
        let mut unsettled = parked_x(&a);
        tell(&mut unsettled, child_start(X, at));
        assert_eq!(phase(&unsettled), Phase::Unsettled { since }, "{at:?}");
        assert_eq!(tracked(&unsettled).as_of, Some(H + 2000), "{at:?}");
        let settles = since.is_some();
        for st in [unsettled.clone(), restarted(&unsettled)] {
            let mut running = st.clone();
            tell(
                &mut running,
                child_stop(B, vec![sub(X, X_NAME), other()], Some(H + 3000)),
            );
            let expected = match settles {
                true => Phase::Live,
                false => Phase::Unsettled { since },
            };
            assert_eq!(phase(&running), expected, "{at:?}");
            assert_eq!(handed(&mut running, &a), Verdict::Park, "{at:?}");

            let mut parked_again = st.clone();
            tell(
                &mut parked_again,
                child_stop(B, vec![other(), shell("s2")], Some(H + 3000)),
            );
            assert_eq!(phase(&parked_again), Phase::Unsettled { since }, "{at:?}");
            assert_eq!(handed(&mut parked_again, &a), Verdict::Park, "{at:?}");

            let mut gone = st.clone();
            assert_eq!(
                verdict(gone.step(
                    listed_at(vec![other(), shell("s2")], H + 3000),
                    &facts(T0, &s, &a)
                )),
                Verdict::Park,
                "{at:?}"
            );
            let expected = match settles {
                true => parked_on(&["s2"], false),
                false => Phase::Unsettled { since },
            };
            assert_eq!(phase(&gone), expected, "{at:?}");

            tell(
                &mut running,
                child_stop(X, vec![sub(X, X_NAME), other()], Some(H + 4000)),
            );
            assert_eq!(running.attendances.is_empty(), settles, "{at:?}");
            if !settles {
                assert_eq!(handed(&mut running, &a), Verdict::Park, "{at:?}");
            }
        }
    }

    let mut st = parked_x(&a);
    tell(&mut st, child_start(X, Some(H + 2500)));
    assert_eq!(phase(&st), Phase::Live);
    tell(
        &mut st,
        child_stop(B, vec![sub(X, X_NAME), other()], Some(H + 3000)),
    );
    assert_eq!(phase(&st), Phase::Live);
    assert_eq!(handed(&mut st, &a), Verdict::Park);
    tell(
        &mut st,
        child_stop(X, vec![sub(X, X_NAME), other()], Some(H + 4000)),
    );
    assert!(st.attendances.is_empty());
}

/// The clock steps back as X resumes, so its start reads earlier than its
/// park; a stop X's hook took before the step arrives after the start,
/// stamped later than the park. Nothing proves that stop followed the
/// resume, so it ends nothing, before or after a restart. Neither does a
/// stop at the same time as the park, one with no time, or one earlier
/// still. A start with no time leaves the same: no stop at any time ends
/// it. Only `clank attend --clear` or the check-in does.
#[test]
fn review_attendance_unordered_resume_survives_a_delayed_pre_resume_stop() {
    let a = owed("event:a", "read:a");
    for start in [Some(H + 1500), None] {
        let mut st = parked_x(&a);
        tell(&mut st, child_start(X, start));
        assert_eq!(phase(&st), Phase::Unsettled { since: None }, "{start:?}");
        for stopped in [
            Some(H + 2500),
            Some(H + 2000),
            Some(H + 1000),
            None,
            Some(H + 999_999),
        ] {
            for mut next in [st.clone(), restarted(&st)] {
                tell(&mut next, child_stop(X, vec![sub(X, X_NAME)], stopped));
                assert_eq!(
                    handed(&mut next, &a),
                    Verdict::Park,
                    "a stop at {stopped:?} cannot settle a resume at {start:?}"
                );
                assert_eq!(phase(&next), Phase::Unsettled { since: None });
            }
        }
        let mut cleared = st.clone();
        cleared.step(clear_all(), &facts(T0, &session(), &a));
        assert!(matches!(handed(&mut cleared, &a), Verdict::Deliver { .. }));
    }
}

/// A start remembered for an id binds its declaration unsettled when the
/// stop that binds it is not strictly later. The start's time is the
/// barrier, and a stop is no resume, so a stop at that time, an earlier
/// one or one with no time leaves it: the agent's next Stop without the
/// subagent settles it. A start with no time leaves no barrier, and that
/// Stop does not. A strictly later stop parks it at once.
#[test]
fn a_stop_not_later_than_a_remembered_start_binds_unsettled() {
    let s = session();
    let none: [OwedItem; 0] = [];
    let bound = |start, stop| {
        let mut st = SessionState::new(s.clone());
        st.step(declare(X_NAME), &facts(T0, &s, &none));
        tell(&mut st, child_start(X, start));
        tell(
            &mut st,
            child_stop(X, vec![sub(X, X_NAME), shell("s1")], stop),
        );
        assert_eq!(bound_to(&st), [Some(X)]);
        st
    };
    for (start, stop, since) in [
        (Some(H + 5), Some(H + 5), Some(H + 5)),
        (Some(H + 5), Some(H + 4), Some(H + 5)),
        (Some(H + 5), None, Some(H + 5)),
        (None, Some(H + 9), None),
    ] {
        let mut st = bound(start, stop);
        assert_eq!(
            phase(&st),
            Phase::Unsettled { since },
            "{start:?} then {stop:?}"
        );
        let mut again = restarted(&st);
        for st in [&mut st, &mut again] {
            st.step(listed_at(vec![shell("s1")], H + 100), &facts(T0, &s, &none));
            let expected = match since {
                Some(_) => parked_on(&["s1"], false),
                None => Phase::Unsettled { since: None },
            };
            assert_eq!(phase(st), expected, "{start:?} then {stop:?}");
        }
    }
    assert_eq!(
        phase(&bound(Some(H + 5), Some(H + 6))),
        parked_on(&["s1"], false)
    );

    for starts in [[H + 5, H + 9], [H + 9, H + 5]] {
        let mut st = SessionState::new(s.clone());
        st.step(declare(X_NAME), &facts(T0, &s, &none));
        for at in starts {
            tell(&mut st, child_start(X, Some(at)));
        }
        tell(
            &mut st,
            child_stop(X, vec![sub(X, X_NAME), shell("s1")], Some(H + 7)),
        );
        assert!(
            matches!(phase(&st), Phase::Unsettled { .. }),
            "the latest start remembered is the one a stop must follow: {starts:?}"
        );
    }
}

/// A declaration bound to a shell, or to anything but a subagent, ends
/// when its task leaves the listing, whatever else is newly listed beside
/// it: an agent's Stop or a subagent's stop listing new work. Starts and
/// stops reported for its id move nothing.
#[test]
fn review_attendance_of_a_shell_ends_when_that_shell_is_gone() {
    let s = session();
    let none: [OwedItem; 0] = [];
    let bound = || {
        let mut st = SessionState::new(s.clone());
        st.step(declare("run s1"), &facts(T0, &s, &none));
        let s1 = BackgroundTask::from(serde_json::json!({
            "id": "s1", "type": "shell", "status": "running", "description": "run s1"
        }));
        st.step(listed_at(vec![s1.clone()], H + 1000), &facts(T0, &s, &none));
        assert_eq!(bound_to(&st), [Some("s1")]);
        (st, s1)
    };
    let (mut st, s1) = bound();
    st.step(
        listed_at(vec![s1.clone(), shell("s2")], H + 2000),
        &facts(T0, &s, &none),
    );
    assert_eq!(bound_to(&st), [Some("s1")], "still listed");
    st.step(
        listed_at(vec![shell("s2")], H + 3000),
        &facts(T0, &s, &none),
    );
    assert!(
        st.attendances.is_empty(),
        "a shell starts no harness task: it does not follow unrelated s2: {:?}",
        st.attendances
    );

    let (mut st, s1) = bound();
    tell(&mut st, child_start("s1", Some(H + 1500)));
    tell(
        &mut st,
        child_stop(B, vec![s1, sub(B, "another"), shell("s2")], Some(H + 2000)),
    );
    assert_eq!(bound_to(&st), [Some("s1")]);
    tell(
        &mut st,
        child_stop(B, vec![sub(B, "another"), shell("s2")], Some(H + 3000)),
    );
    assert!(st.attendances.is_empty(), "a subagent's listing without it");

    let untyped = BackgroundTask::from(serde_json::json!({
        "id": "u1", "status": "running", "description": "run u1"
    }));
    let mut st = SessionState::new(s.clone());
    st.step(listed_at(vec![shell("old")], H), &facts(T0, &s, &none));
    st.step(declare("run u1"), &facts(T0, &s, &none));
    st.step(
        listed_at(vec![shell("old"), untyped], H + 1000),
        &facts(T0, &s, &none),
    );
    assert_eq!(bound_to(&st), [Some("u1")]);
    st.step(
        listed_at(vec![shell("s2")], H + 2000),
        &facts(T0, &s, &none),
    );
    assert!(
        st.attendances.is_empty(),
        "a task the listing gave no kind is not a subagent"
    );
}

mod legacy_bytes {
    /// A declaration bound to a subagent and an `"unsettled"` phase, as the
    /// build that first kept phases wrote them: no kind on either, and no
    /// barrier on the phase.
    pub const KINDLESS: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendances":[{"work":{"Declared":{"name":"Implement X","task":"x1","tracked":{"baseline":["m1"],"as_of":2000002000,"phase":{"parked":{"on":[{"id":"s1","kind":"shell"}],"sure":false}}}}},"desc":"Implement X","started":1000000,"since":1000000,"expect":600000,"check_in":"Armed"},{"work":{"Declared":{"name":"Implement Y","task":"y1","tracked":{"as_of":2000002000,"phase":"unsettled"}}},"desc":"Implement Y","started":1000000,"since":1000000,"expect":600000,"check_in":"Armed"}],"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
}

/// A bound declaration with no kind was written before kinds were kept.
/// One the subagent table touched, as the build that first kept phases
/// wrote it, keeps that table: its bare `"unsettled"` loads with no
/// barrier and outlasts any listing, and its park on s1 holds while s1 is
/// listed, before and after a restore. Once s1 has gone and the unsettled
/// one is cleared, what they quieted is delivered. One the table never
/// touched, from before it, ends when its task leaves the listing.
#[test]
fn a_declaration_bound_without_a_kind_keeps_the_rule_its_record_shows() {
    let s = session();
    let a = owed("event:a", "read:a");
    let on_s1 = Phase::Parked {
        on: vec![HeldTask {
            id: "s1".into(),
            kind: Some("shell".into()),
            description: None,
        }],
        sure: false,
    };
    let restored = restore(legacy_bytes::KINDLESS);
    assert_eq!(phase_of(&restored, Y), Phase::Unsettled { since: None });
    for mut st in [restored.clone(), restarted(&restored)] {
        assert_eq!(
            verdict(st.step(listed_at(vec![shell("s1")], H + 3000), &facts(T0, &s, &a))),
            Verdict::Park
        );
        assert_eq!(bound_to(&st), [Some(X), Some(Y)]);
        assert_eq!(phase_of(&st, X), on_s1);
        assert_eq!(phase_of(&st, Y), Phase::Unsettled { since: None });
        let mut again = restarted(&st);
        for st in [&mut st, &mut again] {
            st.step(listed_at(vec![], H + 4000), &facts(T0, &s, &a));
            assert_eq!(bound_to(st), [Some(Y)], "x1's work has gone");
            assert_eq!(handed(st, &a), Verdict::Park, "y1 still holds it");
            st.step(
                Event::Attend(AttendRequest::Clear {
                    session: session(),
                    which: Clearing::Named(Y_NAME.into()),
                }),
                &facts(T0, &s, &a),
            );
            let (_, identities) = presented(handed(st, &a));
            assert_eq!(identities, ["event:a"]);
        }
    }

    let mut st = restore(old_bytes::BOUND);
    assert_eq!(*tracked(&st), Tracked::default());
    st.step(listed_at(vec![shell("s1")], H), &facts(T0, &s, &a));
    assert!(st.attendances.is_empty(), "untouched: ended on absence");
}

#[test]
fn review_legacy_unsettled_attendance_is_not_ended_by_missing_kind() {
    let s = session();
    let none: [OwedItem; 0] = [];
    let mut st = restore(legacy_bytes::KINDLESS);
    assert_eq!(phase_of(&st, Y), Phase::Unsettled { since: None });
    st.step(
        listed_at(vec![shell("s1")], H + 3000),
        &facts(T0, &s, &none),
    );
    assert!(
        st.attendances.iter().any(|e| e.task() == Some(Y)),
        "a stored uncertain child resume must not be forgotten merely because the old build omitted kind"
    );
}

/// Arriving later is not being later. Once a start is applied, a stop of
/// the subagent's whose hook began before it, at the same time, or at no
/// stated time changes nothing, and neither does a Stop of the agent's
/// taken before it that lacks the subagent. Each holds after a restart.
#[test]
fn a_report_from_before_the_last_applied_changes_nothing() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut resumed = parked_x(&a);
    tell(&mut resumed, child_start(X, Some(H + 3000)));
    assert_eq!(phase(&resumed), Phase::Live);
    for st in [resumed.clone(), restarted(&resumed)] {
        for at in [Some(H + 2500), Some(H + 3000), None] {
            let mut st = st.clone();
            tell(&mut st, child_stop(X, vec![sub(X, X_NAME)], at));
            assert_eq!(phase(&st), Phase::Live, "a stop at {at:?}");
            assert_eq!(tracked(&st).as_of, Some(H + 3000));
        }
        let mut st = st.clone();
        assert_eq!(
            verdict(st.step(listed_at(vec![], H + 2500), &facts(T0, &s, &a))),
            Verdict::Park
        );
        assert_eq!(phase(&st), Phase::Live, "an earlier Stop of the agent's");
    }
}

/// A harness with a listing and no child hooks: a subagent gone from the
/// agent's Stop is parked on whatever that Stop lists that is newer than
/// it, never sure, and ended when it lists nothing newer.
#[test]
fn without_child_hooks_absence_at_the_agents_stop_parks_or_ends() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = bound_x(&a);
    assert_eq!(
        verdict(st.step(listed_at(vec![shell("s1")], H + 2000), &facts(T0, &s, &a))),
        Verdict::Park
    );
    assert_eq!(phase(&st), parked_on(&["s1"], false));
    let (_, identities) = presented(verdict(
        st.step(listed_at(vec![], H + 3000), &facts(T0, &s, &a)),
    ));
    assert!(st.attendances.is_empty());
    assert_eq!(identities, ["event:a"]);

    let mut st = bound_x(&a);
    st.step(listed_at(vec![], H + 2000), &facts(T0, &s, &a));
    assert!(st.attendances.is_empty(), "nothing newer was listed");
}

/// A monitor restarted between the park and the resume holds the same
/// attendance: its baseline, what it is parked on and the time it stands
/// at, with what the binding has seen and the starts it remembers.
#[test]
fn a_restart_keeps_a_parked_attendance_and_what_orders_it() {
    let a = owed("event:a", "read:a");
    let mut st = parked_x(&a);
    tell(&mut st, child_start("unbound", Some(H + 2100)));
    let mut again = restarted(&st);
    assert_eq!(again.attendances, st.attendances);
    assert_eq!(again.seen, st.seen);
    assert_eq!(again.starts, st.starts);
    assert_eq!(handed(&mut again, &a), Verdict::Park);
    tell(&mut again, child_start(X, Some(H + 3000)));
    assert_eq!(phase(&again), Phase::Live);
    tell(
        &mut again,
        child_stop(X, vec![sub(X, X_NAME)], Some(H + 4000)),
    );
    assert!(again.attendances.is_empty());
}

/// A child's report is evidence about a declaration and nothing else: it
/// leaves the Stop the agent reported, its presentation in flight, its
/// repeat count, its lost park and what it has received exactly as they
/// were. One from another session changes nothing at all.
#[test]
fn a_childs_report_touches_only_what_is_attended() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = bound_x(&a);
    st.unwatched = Some(Unwatched { since: T0 });
    let before = st.clone();
    assert!(before.stopped.is_some());
    tell(
        &mut st,
        child_stop(X, vec![sub(X, X_NAME), shell("s1")], Some(H + 2000)),
    );
    assert_eq!(phase(&st), parked_on(&["s1"], false));
    tell(&mut st, child_start(X, Some(H + 3000)));
    assert_eq!(
        (
            &st.stopped,
            &st.outstanding,
            &st.repeat,
            &st.resolved,
            st.next_seq,
            &st.received,
            st.unwatched,
            &st.feedback,
        ),
        (
            &before.stopped,
            &before.outstanding,
            &before.repeat,
            &before.resolved,
            before.next_seq,
            &before.received,
            before.unwatched,
            &before.feedback,
        )
    );

    let mut st = before.clone();
    for mut report in [
        child_stop(X, vec![sub(X, X_NAME)], Some(H + 2000)),
        child_start(X, Some(H + 2000)),
    ] {
        report.session = other_session();
        assert!(!st.child(&report, &facts(T0, &s, &a)));
    }
    assert_eq!(st, before);
}

/// A stop of a subagent nothing is bound to changes no attendance, and
/// its listing still binds a pending declaration it shows and is
/// remembered as seen. It counts no miss against a declaration it does
/// not show: only the agent's own Stops do.
#[test]
fn an_undeclared_childs_stop_still_binds_and_is_remembered() {
    let s = session();
    let none: [OwedItem; 0] = [];
    let mut st = SessionState::new(s.clone());
    st.step(declare(X_NAME), &facts(T0, &s, &none));
    for at in [H + 100, H + 200, H + 300] {
        tell(
            &mut st,
            child_stop(B, vec![sub(B, "another subagent")], Some(at)),
        );
    }
    assert_eq!(bound_to(&st), [None], "no miss was counted");
    assert!(st.feedback.is_empty());

    tell(
        &mut st,
        child_stop(
            B,
            vec![sub(B, "another subagent"), sub(X, X_NAME), shell("s1")],
            Some(H + 1000),
        ),
    );
    assert_eq!(bound_to(&st), [Some(X)]);
    assert_eq!(phase(&st), Phase::Live);
    assert_eq!(tracked(&st).baseline, [B]);
    assert_eq!(st.seen.get("s1"), Some(&Some(H + 1000)));
}

mod old_bytes {
    /// A declaration bound to its task and a Stop, as the last build
    /// before listings were remembered wrote them.
    pub const BOUND: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendances":[{"work":{"Declared":{"name":"Implement X","task":"x1"}},"desc":"Implement X","started":1000000,"since":1000000,"expect":600000,"check_in":"Armed"}],"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null,"stopped":{"turn":"prompt-1","position":40,"at":1000000}}"#;
}

/// State saved before any of this was kept loads, and is written back
/// byte for byte while nothing new is known: its declaration is a task
/// running with no baseline, nothing has been seen, and its Stop has no
/// hook time or listing. It carries no kind, so it keeps the rule it was
/// bound under: the agent's next Stop without the task ends it, whatever
/// else is listed.
#[test]
fn state_saved_before_listings_were_remembered_loads() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = restore(old_bytes::BOUND);
    assert_eq!(
        serde_json::to_string(&st.persisted()).unwrap(),
        old_bytes::BOUND
    );
    assert_eq!(*tracked(&st), Tracked::default());
    assert_eq!(phase(&st), Phase::Live);
    assert!(st.seen.is_empty() && st.starts.is_empty());
    let stop = st.stopped.clone().unwrap();
    assert_eq!((stop.hook_started, stop.listed), (None, None));

    let (_, identities) = presented(verdict(
        st.step(listed_at(vec![shell("s1")], H), &facts(T0, &s, &a)),
    ));
    assert!(st.attendances.is_empty());
    assert_eq!(identities, ["event:a"]);
}
