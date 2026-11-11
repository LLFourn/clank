use super::*;

mod subagents;

const MINUTE: Ms = 60_000;
const T0: Ms = 1_000_000;
const SUBAGENT_NAME: &str = "capture-probe sleeper";

struct Alive(Presence);
impl Liveness for Alive {
    fn presence(&self, _: &ShellProof) -> Presence {
        self.0
    }
}
static LIVE: Alive = Alive(Presence::Live);
static DEAD: Alive = Alive(Presence::Gone);
static UNSEEN: Alive = Alive(Presence::Unknown);

fn session() -> SessionId {
    SessionId::parse("00000000-0000-0000-0000-000000000000").unwrap()
}

fn other_session() -> SessionId {
    SessionId::parse("11111111-1111-1111-1111-111111111111").unwrap()
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

fn dead<'a>(now: Ms, binding: &'a SessionId, owed: &'a [OwedItem]) -> Facts<'a> {
    Facts {
        liveness: &DEAD,
        ..facts(now, binding, owed)
    }
}

/// A hook time later than every one handed out before it: each report a
/// test builds is its session's latest unless the test says otherwise.
fn later() -> Option<Ms> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(T0);
    Some(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn report(tool: Tool) -> TurnEndReport {
    TurnEndReport {
        tool,
        session: session(),
        place: TurnPlace::default(),
        tasks: TaskSnapshot::Unavailable,
        hook_started: later(),
    }
}

/// Claude's Stop exactly as captured under `tests/fixtures/stop_hook/`.
fn claude_stop(fixture: &str) -> TurnEndReport {
    let path = format!(
        "{}/tests/fixtures/stop_hook/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    );
    let input: HookInput = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    TurnEndReport {
        tasks: TaskSnapshot::of(&input),
        ..report(Tool::Claude)
    }
}

const SUBAGENT_ID: &str = "a25eee2eef81caf3e";

/// A Claude-shaped listing of live subagents, `(id, name)`.
fn listing(tasks: &[(&str, &str)]) -> TaskSnapshot {
    TaskSnapshot::Known(
        tasks
            .iter()
            .map(|(id, name)| {
                BackgroundTask::from(serde_json::json!({
                    "id": id, "type": "subagent", "status": "running", "description": name,
                    "agent_type": "general-purpose"
                }))
            })
            .collect(),
    )
}

fn listed_stop(tasks: &[(&str, &str)]) -> Event {
    Event::TurnEnd(TurnEndReport {
        tasks: listing(tasks),
        ..report(Tool::Claude)
    })
}

/// A Stop whose newest turn is the person's: a prompt typed with no
/// delivery behind it.
fn persons_stop(turn: &str, tasks: TaskSnapshot) -> Event {
    Event::TurnEnd(TurnEndReport {
        place: TurnPlace {
            turn: Some(turn.into()),
            position: None,
            human_turn: true,
            read_to: None,
        },
        tasks,
        ..report(Tool::Claude)
    })
}

fn bound_to(st: &SessionState) -> Vec<Option<&str>> {
    st.attendances.iter().map(Entry::task).collect()
}

fn stop() -> Event {
    Event::TurnEnd(report(Tool::Claude))
}

fn declare(name: &str) -> Event {
    Event::Attend(AttendRequest::Declare {
        session: session(),
        name: name.into(),
        desc: None,
        expect: 10 * MINUTE,
    })
}

fn proof_of(pid: i32) -> ShellProof {
    ShellProof {
        pid,
        token: format!("mac1:{pid}:1"),
    }
}

fn run(desc: &str, pid: i32) -> Event {
    Event::Attend(AttendRequest::Run {
        session: session(),
        proof: proof_of(pid),
        desc: desc.into(),
        expect: 10 * MINUTE,
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

fn attend_reply(out: Output) -> AttendReply {
    match out {
        Output::Attend(r) => r,
        other => panic!("not an attend reply: {other:?}"),
    }
}

/// A session with the captured subagent declared just now.
fn standing(s: &SessionId) -> SessionState {
    let mut st = SessionState::new(s.clone());
    assert_eq!(
        attend_reply(st.step(declare(SUBAGENT_NAME), &facts(T0, s, &[]))),
        AttendReply::Accepted
    );
    st
}

fn entry(st: &SessionState) -> &Entry {
    match st.attendances.as_slice() {
        [only] => only,
        other => panic!("not one entry: {other:?}"),
    }
}

fn names(st: &SessionState) -> Vec<&str> {
    st.attendances.iter().map(Entry::name).collect()
}

fn clear_all() -> Event {
    Event::Attend(AttendRequest::Clear {
        session: session(),
        which: Clearing::All,
    })
}

/// The check-in `v` carries, if it carries one.
fn check_in_of(v: &Verdict) -> Option<&[CheckInLine]> {
    match v {
        Verdict::Deliver { notices, .. } => notices.iter().find_map(|n| match n {
            Notice::CheckIn(lines) => Some(lines.as_slice()),
            _ => None,
        }),
        _ => None,
    }
}

/// The presentation carrying `v`'s check-in, and the names it is for.
fn checked_in(v: Verdict) -> (PresentationId, Vec<String>) {
    let names = check_in_of(&v)
        .unwrap_or_else(|| panic!("no check-in: {v:?}"))
        .iter()
        .map(|l| l.name.clone())
        .collect();
    (presented(v).0, names)
}

/// The report a park holds, stepped again.
fn parked() -> Event {
    Event::Parked(report(Tool::Claude))
}

/// A compaction's report: a new report of the park, unplaced.
fn compacted() -> Event {
    Event::Compacted(report(Tool::Claude))
}

fn owed(identity: &str, receipt: &str) -> [OwedItem; 1] {
    [OwedItem {
        identity: identity.into(),
        receipts: vec![receipt.into()],
    }]
}

fn deliver(st: &mut SessionState, f: &Facts<'_>) -> (PresentationId, u32) {
    match verdict(st.step(Event::TurnEnd(report(Tool::Claude)), f)) {
        Verdict::Deliver {
            presentation,
            repeat,
            ..
        } => (presentation, repeat),
        other => panic!("{other:?}"),
    }
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

/// `item` delivered and received, then `attend` made in the turn that
/// delivery started.
fn received_then(s: &SessionId, item: &[OwedItem], attend: Event) -> SessionState {
    let mut st = SessionState::new(s.clone());
    let (p, _) = deliver(&mut st, &facts(T0, s, item));
    committed(st.step(Event::Delivered(p), &facts(T0, s, item)));
    st.step(attend, &facts(T0, s, item));
    st
}

/// A turn end with nothing attended is the plain owed-work decision for
/// every tool: there is no listing to read and so nothing to name.
#[test]
fn a_turn_end_with_nothing_attended_carries_no_notice_for_any_tool() {
    let s = session();
    let a = owed("event:a", "read:a");
    for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
        let mut st = SessionState::new(s.clone());
        assert_eq!(
            verdict(st.step(Event::TurnEnd(report(tool)), &facts(T0, &s, &[]))),
            Verdict::Park,
            "{tool:?}"
        );
        let mut st = SessionState::new(s.clone());
        let v = verdict(st.step(Event::TurnEnd(report(tool)), &facts(T0, &s, &a)));
        assert!(
            matches!(&v, Verdict::Deliver { identities, .. } if identities == &["event:a"]),
            "{tool:?}: {v:?}"
        );
    }
}

/// A declaration is in force the moment it is made: no Stop proves it.
#[test]
fn a_declaration_takes_effect_at_once_for_every_tool() {
    let s = session();
    let a = owed("event:a", "read:a");
    for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
        let st = received_then(&s, &a, declare(SUBAGENT_NAME));
        assert_eq!(entry(&st).window.check_in, CheckIn::Armed);
        assert_eq!(st.next_deadline(), Some(T0 + 10 * MINUTE));
        assert_eq!(
            st.recovery_quiet(&facts(T0 + MINUTE, &s, &a)),
            Some(Quiet::Attending),
            "{tool:?}"
        );
        let mut st = st;
        assert_eq!(
            verdict(st.step(Event::TurnEnd(report(tool)), &facts(T0 + MINUTE, &s, &a))),
            Verdict::Park,
            "{tool:?}: received work stays quiet at the turn's own end"
        );
    }
}

/// A declared subagent holds while Claude lists it, across turns the
/// person starts and deliveries of new work alike: no turn start ends it.
/// Claude's next listing without it ends it, and what it quieted wakes the
/// agent again.
#[test]
fn a_declared_subagent_holds_while_listed_and_ends_when_unlisted() {
    let s = session();
    let a = owed("event:a", "read:a");
    let ab = [a[0].clone(), owed("message:b", "read:b")[0].clone()];
    let mut st = received_then(&s, &a, declare(SUBAGENT_NAME));
    let running = || Event::TurnEnd(claude_stop("stop_subagent_running.json"));
    assert_eq!(
        verdict(st.step(running(), &facts(T0 + MINUTE, &s, &a))),
        Verdict::Park
    );
    assert_eq!(bound_to(&st), [Some(SUBAGENT_ID)], "resolved to its task");

    for (at, turn) in [(2, "prompt-1"), (3, "prompt-2")] {
        let TurnEndReport { tasks, .. } = claude_stop("stop_subagent_running.json");
        assert_eq!(
            verdict(st.step(persons_stop(turn, tasks), &facts(T0 + at * MINUTE, &s, &a))),
            Verdict::Park,
            "the person's turn {turn} ends nothing"
        );
    }
    let (message, identities) = presented(verdict(
        st.step(running(), &facts(T0 + 4 * MINUTE, &s, &ab)),
    ));
    assert_eq!(identities, ["message:b"], "new work still wakes");
    committed(st.step(Event::Delivered(message), &facts(T0 + 4 * MINUTE, &s, &ab)));
    assert_eq!(
        bound_to(&st),
        [Some(SUBAGENT_ID)],
        "the turn a delivery starts ends nothing"
    );
    assert_eq!(
        verdict(st.step(running(), &facts(T0 + 5 * MINUTE, &s, &ab))),
        Verdict::Park
    );

    let finished = Event::TurnEnd(claude_stop("stop_subagent_finished.json"));
    let (_, identities) = presented(verdict(st.step(finished, &facts(T0 + 6 * MINUTE, &s, &ab))));
    assert!(st.attendances.is_empty(), "the listing no longer has it");
    assert_eq!(
        identities,
        ["event:a", "message:b"],
        "what it quieted wakes"
    );
}

/// A listing that was not sent says nothing: no number of Stops without
/// one ends a declaration or counts against its match. Only a known
/// listing, even an empty one, does.
#[test]
fn an_omitted_listing_is_not_an_empty_one() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut bound = received_then(&s, &a, declare(SUBAGENT_NAME));
    bound.step(
        listed_stop(&[(SUBAGENT_ID, SUBAGENT_NAME)]),
        &facts(T0, &s, &a),
    );
    let mut pending = received_then(&s, &a, declare("not started yet"));
    for at in 1..=5 {
        for st in [&mut bound, &mut pending] {
            assert_eq!(
                verdict(st.step(stop(), &facts(T0 + at * MINUTE, &s, &a))),
                Verdict::Park,
                "omitted listing {at}"
            );
        }
    }
    assert_eq!(bound_to(&bound), [Some(SUBAGENT_ID)]);
    assert!(matches!(
        pending.attendances[0].work,
        Attending::Declared { misses: 0, .. }
    ));
    let (_, identities) = presented(verdict(
        bound.step(listed_stop(&[]), &facts(T0 + 6 * MINUTE, &s, &a)),
    ));
    assert_eq!(identities, ["event:a"]);
    assert!(bound.attendances.is_empty(), "an empty listing ends it");
}

/// A declaration is bound to the task it resolved to, not to its name: a
/// later task reusing the description is not it. Declaring the name again
/// asks the next listing which task it means now.
#[test]
fn a_declaration_is_its_task_not_its_description() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, declare(SUBAGENT_NAME));
    st.step(listed_stop(&[("first", SUBAGENT_NAME)]), &facts(T0, &s, &a));
    assert_eq!(bound_to(&st), [Some("first")]);
    let (_, identities) = presented(verdict(st.step(
        listed_stop(&[("second", SUBAGENT_NAME)]),
        &facts(T0 + MINUTE, &s, &a),
    )));
    assert_eq!(identities, ["event:a"], "the first task ended");
    assert!(st.attendances.is_empty(), "the second is nobody's");

    let mut st = received_then(&s, &a, declare(SUBAGENT_NAME));
    st.step(listed_stop(&[("first", SUBAGENT_NAME)]), &facts(T0, &s, &a));
    st.step(declare(SUBAGENT_NAME), &facts(T0 + MINUTE, &s, &a));
    assert_eq!(bound_to(&st), [Some("first"), None], "a renewal is pending");
    st.step(
        listed_stop(&[("first", SUBAGENT_NAME)]),
        &facts(T0 + 2 * MINUTE, &s, &a),
    );
    assert_eq!(bound_to(&st), [Some("first")], "same task: merged");
    assert_eq!(
        entry(&st).window.since,
        T0 + MINUTE,
        "on the renewal's clock"
    );
    st.step(declare(SUBAGENT_NAME), &facts(T0 + 3 * MINUTE, &s, &a));
    assert_eq!(
        verdict(st.step(
            listed_stop(&[("second", SUBAGENT_NAME)]),
            &facts(T0 + 4 * MINUTE, &s, &a),
        )),
        Verdict::Park
    );
    assert_eq!(
        bound_to(&st),
        [Some("second")],
        "the renewal took the new task"
    );
}

/// A listed task nobody declared is ignored: it attends nothing, raises
/// no notice and wakes nobody, whatever its kind.
#[test]
fn a_listed_task_nobody_declared_is_ignored() {
    let s = session();
    let a = owed("event:a", "read:a");
    let monitor = Event::TurnEnd(claude_stop("stop_monitor_task_running.json"));
    let subagent = Event::TurnEnd(claude_stop("stop_subagent_running.json"));
    for event in [monitor, subagent] {
        let mut st = SessionState::new(s.clone());
        assert_eq!(
            verdict(st.step(event.clone(), &facts(T0, &s, &[]))),
            Verdict::Park,
            "nothing owed: nothing wakes"
        );
        assert!(st.attendances.is_empty());
        let v = verdict(st.step(event, &facts(T0, &s, &a)));
        let Verdict::Deliver {
            identities,
            notices,
            ..
        } = &v
        else {
            panic!("{v:?}")
        };
        assert_eq!(identities, &["event:a"]);
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(st.recovery_quiet(&facts(T0, &s, &a)), None);
    }
}

/// A name that matches nothing listed is given one more listing (the
/// work may start in the next turn), then dropped with one notice naming
/// what is listed. Until then it stands as declared.
#[test]
fn a_declaration_nothing_listed_matches_is_dropped_with_one_notice() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, declare("sleeperr"));
    let listed = || listed_stop(&[(SUBAGENT_ID, SUBAGENT_NAME)]);
    assert_eq!(
        verdict(st.step(listed(), &facts(T0 + MINUTE, &s, &a))),
        Verdict::Park,
        "one miss is allowed"
    );
    let v = verdict(st.step(listed(), &facts(T0 + 2 * MINUTE, &s, &a)));
    let Verdict::Deliver {
        identities,
        notices,
        ..
    } = v
    else {
        panic!("{v:?}")
    };
    assert_eq!(identities, ["event:a"], "it no longer quiets anything");
    assert_eq!(
        notices,
        [Notice::NoMatch {
            name: "sleeperr".into(),
            live: vec![TaskSummary {
                id: Some(SUBAGENT_ID.into()),
                kind: Some("subagent".into()),
                description: Some(SUBAGENT_NAME.into()),
            }],
        }]
    );
    assert!(st.attendances.is_empty());

    let mut quiet = SessionState::new(s.clone());
    quiet.step(declare("sleeperr"), &facts(T0, &s, &[]));
    quiet.step(listed(), &facts(T0, &s, &[]));
    assert!(matches!(
        verdict(quiet.step(listed(), &facts(T0, &s, &[]))),
        Verdict::Notify(n) if matches!(n.as_slice(), [Notice::NoMatch { .. }])
    ));
    assert_eq!(
        verdict(quiet.step(listed(), &facts(T0, &s, &[]))),
        Verdict::Park,
        "reported once"
    );
}

/// A name several listed tasks carry binds to none of them.
#[test]
fn a_name_several_listed_tasks_carry_is_dropped_with_a_notice() {
    let s = session();
    let mut st = standing(&s);
    let v = verdict(st.step(
        listed_stop(&[("a1", SUBAGENT_NAME), ("a2", SUBAGENT_NAME)]),
        &facts(T0, &s, &[]),
    ));
    let Verdict::Notify(notices) = v else {
        panic!("{v:?}")
    };
    let [Notice::Ambiguous { name, matches }] = notices.as_slice() else {
        panic!("{notices:?}")
    };
    assert_eq!(name, SUBAGENT_NAME);
    assert_eq!(matches.len(), 2);
    assert!(st.attendances.is_empty());
}

/// Without a listing (Codex, OpenCode) a declaration outlives every turn,
/// whatever starts it, is checked in on once at `--expect` without ending,
/// and ends only when the agent clears it.
#[test]
fn without_a_listing_only_a_clear_ends_a_declaration() {
    let s = session();
    let a = owed("event:a", "read:a");
    let ab = [a[0].clone(), owed("message:b", "read:b")[0].clone()];
    let mut st = received_then(&s, &a, declare("reviewer"));
    let codex = || Event::TurnEnd(report(Tool::Codex));
    for at in 1..=3 {
        assert_eq!(
            verdict(st.step(codex(), &facts(T0 + at * MINUTE, &s, &a))),
            Verdict::Park,
            "turn {at}"
        );
    }
    let (message, _) = presented(verdict(st.step(codex(), &facts(T0 + 4 * MINUTE, &s, &ab))));
    committed(st.step(Event::Delivered(message), &facts(T0 + 4 * MINUTE, &s, &ab)));
    assert_eq!(names(&st), ["reviewer"], "a delivered message ends nothing");

    let due = st.next_deadline().unwrap();
    let (p, checked) = checked_in(verdict(st.step(codex(), &facts(due, &s, &ab))));
    assert_eq!(checked, ["reviewer"]);
    committed(st.step(Event::Delivered(p), &facts(due, &s, &ab)));
    let again = verdict(st.step(codex(), &facts(due + MINUTE, &s, &ab)));
    assert_eq!(check_in_of(&again), None, "once");
    assert_eq!(names(&st), ["reviewer"], "a check-in is not an end");

    let cleared = Event::Attend(AttendRequest::Clear {
        session: s.clone(),
        which: Clearing::Named("reviewer".into()),
    });
    assert_eq!(
        attend_reply(st.step(cleared, &facts(due + MINUTE, &s, &ab))),
        AttendReply::Cleared { removed: 1 }
    );
    assert!(st.attendances.is_empty());
}

/// Declaration feedback, made to fail one way or the other: a name that
/// two Claude listings never match, or one that two listed tasks carry.
#[derive(Clone, Copy, Debug)]
enum Mismatch {
    NoMatch,
    Ambiguous,
}

impl Mismatch {
    /// Two Stops that drop the declaration `"typo"`.
    fn stops(self) -> [Event; 2] {
        match self {
            Self::NoMatch => [listed_stop(&[]), listed_stop(&[])],
            Self::Ambiguous => [
                listed_stop(&[]),
                listed_stop(&[("a1", "typo"), ("a2", "typo")]),
            ],
        }
    }

    fn is(self, notices: &[Notice]) -> bool {
        match (self, notices) {
            (Self::NoMatch, [Notice::NoMatch { name, .. }])
            | (Self::Ambiguous, [Notice::Ambiguous { name, .. }]) => name == "typo",
            _ => false,
        }
    }
}

type Gate = fn(Facts<'_>) -> Facts<'_>;

/// What holds a wake back holds declaration feedback back: a pending
/// question, a block, auto off, or a person's command waiting for the idle
/// prompt. The listing still ends the declaration
/// meanwhile, and the feedback is held, not lost: the first Stop or parked
/// re-step the gates allow wakes with it, and only once.
#[test]
fn declaration_feedback_waits_for_the_wake_gate() {
    let s = session();
    let gates: [(&str, Gate); 4] = [
        ("asking", |f| Facts { asking: true, ..f }),
        ("blocked", |f| Facts { blocked: true, ..f }),
        ("auto off", |f| Facts {
            auto: Ok(AutoMode::Off),
            ..f
        }),
        ("a person's command", |f| Facts {
            user_command: true,
            ..f
        }),
    ];
    for mismatch in [Mismatch::NoMatch, Mismatch::Ambiguous] {
        for (gate, gated) in gates {
            let mut st = SessionState::new(s.clone());
            st.step(declare("typo"), &facts(T0, &s, &[]));
            for event in mismatch.stops() {
                let v = verdict(st.step(event, &gated(facts(T0, &s, &[]))));
                assert!(
                    matches!(
                        v,
                        Verdict::Park
                            | Verdict::Silent(SilentWhy::AutoOff | SilentWhy::UserCommand)
                    ),
                    "{mismatch:?} {gate}: no wake while gated: {v:?}"
                );
            }
            assert!(
                st.attendances.is_empty(),
                "{mismatch:?} {gate}: still ended"
            );
            let held = st.persisted();
            assert_eq!(held.feedback.len(), 1, "{mismatch:?} {gate}: held");
            let back: Persisted =
                serde_json::from_str(&serde_json::to_string(&held).unwrap()).unwrap();
            let mut st = SessionState::restored(back);
            let v = verdict(st.step(
                Event::Parked(report(Tool::Claude)),
                &facts(T0 + MINUTE, &s, &[]),
            ));
            assert!(
                matches!(&v, Verdict::Notify(n) if mismatch.is(n)),
                "{mismatch:?} {gate}: the gate lifted, across a restart: {v:?}"
            );
            assert_eq!(
                verdict(st.step(stop(), &facts(T0 + MINUTE, &s, &[]))),
                Verdict::Park,
                "{mismatch:?} {gate}: once"
            );
        }
    }
}

/// Held feedback rides a wake the agent gets anyway, gated or not: owed
/// work presented while a question is pending carries it.
#[test]
fn held_feedback_rides_a_presentation_that_wakes_anyway() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    st.step(declare("typo"), &facts(T0, &s, &[]));
    let asking = |owed| Facts {
        asking: true,
        ..facts(T0, &s, owed)
    };
    for event in Mismatch::NoMatch.stops() {
        assert_eq!(verdict(st.step(event, &asking(&[]))), Verdict::Park);
    }
    let v = verdict(st.step(stop(), &asking(&a)));
    assert!(
        matches!(&v, Verdict::Deliver { notices, .. } if Mismatch::NoMatch.is(notices)),
        "{v:?}"
    );
    assert!(st.feedback.is_empty());
}

/// Grok's registered waiter already carries the wake, so feedback would
/// make one of its own: it does only through the wake gate, and the
/// waiter verdict itself never changes.
#[test]
fn a_native_stops_feedback_waits_for_the_wake_gate() {
    let s = session();
    let none = || grok_listing(serde_json::json!([]));
    let twins = || {
        grok_listing(serde_json::json!([
            {"id": "g1", "type": "subagent", "status": "running", "description": "typo"},
            {"id": "g2", "type": "subagent", "status": "running", "description": "typo"}
        ]))
    };
    for mismatch in [Mismatch::NoMatch, Mismatch::Ambiguous] {
        for gate in ["asking", "blocked", "a person's command"] {
            let gated = || Facts {
                asking: gate == "asking",
                blocked: gate == "blocked",
                user_command: gate == "a person's command",
                ..facts(T0, &s, &[])
            };
            let mut st = SessionState::new(s.clone());
            st.step(declare("typo"), &facts(T0, &s, &[]));
            let second = match mismatch {
                Mismatch::NoMatch => none(),
                Mismatch::Ambiguous => twins(),
            };
            for tasks in [none(), second] {
                assert_eq!(
                    verdict(st.step(native(&s, true, tasks), &gated())),
                    Verdict::Silent(SilentWhy::WaitArmed),
                    "{mismatch:?} {gate}"
                );
            }
            assert!(st.attendances.is_empty());
            let v = verdict(st.step(native(&s, true, none()), &facts(T0, &s, &[])));
            assert!(
                matches!(&v, Verdict::Notify(n) if mismatch.is(n)),
                "{mismatch:?} {gate}: gates open: {v:?}"
            );
            assert_eq!(
                verdict(st.step(native(&s, true, none()), &facts(T0, &s, &[]))),
                Verdict::Silent(SilentWhy::WaitArmed),
                "{mismatch:?} {gate}: once, and the waiter verdict is back"
            );
        }
    }
}

/// One listed match with no id could never be told ended from any other
/// task: the declaration is dropped at once, saying so, not as a miss.
#[test]
fn a_match_with_no_id_is_dropped_as_untrackable() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(declare("typo"), &facts(T0, &s, &[]));
    let nameless = TaskSnapshot::Known(vec![BackgroundTask::from(serde_json::json!({
        "type": "subagent", "status": "running", "description": "typo"
    }))]);
    let v = verdict(st.step(
        Event::TurnEnd(TurnEndReport {
            tasks: nameless,
            ..report(Tool::Claude)
        }),
        &facts(T0, &s, &[]),
    ));
    assert!(
        matches!(&v, Verdict::Notify(n) if matches!(n.as_slice(),
            [Notice::Untrackable { name, task }] if name == "typo" && task.id.is_none())),
        "{v:?}"
    );
    assert!(st.attendances.is_empty());
}

/// With every gate open, feedback wakes the agent at the Stop that drops
/// the declaration, for either mismatch.
#[test]
fn declaration_feedback_wakes_when_the_gates_allow() {
    let s = session();
    for mismatch in [Mismatch::NoMatch, Mismatch::Ambiguous] {
        let mut st = SessionState::new(s.clone());
        st.step(declare("typo"), &facts(T0, &s, &[]));
        let [first, second] = mismatch.stops();
        assert_eq!(verdict(st.step(first, &facts(T0, &s, &[]))), Verdict::Park);
        let v = verdict(st.step(second, &facts(T0, &s, &[])));
        assert!(matches!(&v, Verdict::Notify(n) if mismatch.is(n)), "{v:?}");
    }
}

/// A run outlives every turn: only its process going ends it.
#[test]
fn a_run_in_flight_keeps_the_agent_attending_across_turns() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, run("test run", 7001));
    for at in 1..5 {
        assert_eq!(
            verdict(st.step(stop(), &facts(T0 + at * MINUTE, &s, &a))),
            Verdict::Park,
            "turn {at}"
        );
    }
    assert_eq!(names(&st), ["test run"]);
    assert_eq!(
        st.recovery_quiet(&facts(T0 + 5 * MINUTE, &s, &a)),
        Some(Quiet::Attending)
    );
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let (_, identities) = presented(verdict(st.step(stop(), &facts(T0 + 6 * MINUTE, &s, &ab))));
    assert_eq!(identities, ["event:b"], "new work still wakes");
}

/// The run's end is its process being gone, by death or by another birth
/// under its pid. Its attendance leaves, and what it quieted is presented
/// at the next turn end like any other owed work. Nothing about it comes
/// from a listing.
#[test]
fn a_gone_run_ends_its_attendance_and_releases_what_it_quieted() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, run("test run", 7001));
    assert!(!st.end_gone_runs(&LIVE));
    assert_eq!(names(&st), ["test run"]);
    assert_eq!(
        st.recovery_quiet(&dead(T0 + 2 * MINUTE, &s, &a)),
        None,
        "a gone run quiets nothing even before it is retired"
    );
    assert!(st.end_gone_runs(&DEAD));
    assert!(st.attendances.is_empty());
    assert!(st.received.is_empty(), "what it quieted is released");
    assert!(!st.end_gone_runs(&DEAD), "never twice");
    let (_, identities) = presented(verdict(st.step(stop(), &facts(T0 + 3 * MINUTE, &s, &a))));
    assert_eq!(identities, ["event:a"]);
}

/// A run's registration is durable: a monitor that restarts finds it and
/// ends it, and a second restart finds nothing.
#[test]
fn a_restart_ends_a_run_that_went_while_nobody_watched() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    let saved = serde_json::to_string(&st.persisted()).unwrap();
    let mut back = SessionState::restored(serde_json::from_str(&saved).unwrap());
    assert!(back.end_gone_runs(&DEAD));
    let saved = serde_json::to_string(&back.persisted()).unwrap();
    let mut again = SessionState::restored(serde_json::from_str(&saved).unwrap());
    assert!(!again.end_gone_runs(&DEAD));
}

/// Killing the command is its end like any other.
#[test]
fn a_killed_run_ends_like_any_other() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(run("server", 7001), &facts(T0, &s, &[]));
    st.step(run("build", 7002), &facts(T0, &s, &[]));
    struct Killed;
    impl Liveness for Killed {
        fn presence(&self, proof: &ShellProof) -> Presence {
            if proof.pid == 7001 {
                Presence::Gone
            } else {
                Presence::Live
            }
        }
    }
    assert!(st.end_gone_runs(&Killed));
    assert_eq!(names(&st), ["build"], "the other run is untouched");
}

/// A declaration still standing at `--expect` is owed one check-in. Its
/// parked hook is handed it as a notice with no work beside it, and only
/// that notice's delivery spends it.
#[test]
fn a_declaration_is_checked_in_on_once_at_its_expectation() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    assert_eq!(due, T0 + 10 * MINUTE);
    assert_eq!(
        verdict(st.step(stop(), &facts(due - 1, &s, &[]))),
        Verdict::Park
    );
    assert_eq!(entry(&st).window.check_in, CheckIn::Armed);
    let v = verdict(st.step(parked(), &facts(due, &s, &[])));
    let lines = check_in_of(&v).expect("checked in at the park").to_vec();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].name, SUBAGENT_NAME);
    assert!(!lines[0].run);
    let (p, identities) = presented(v);
    assert!(identities.is_empty(), "a check-in is not work");
    assert_eq!(
        entry(&st).window.check_in,
        CheckIn::Due,
        "presented is not delivered"
    );
    assert_eq!(st.next_deadline(), None);
    committed(st.step(Event::Delivered(p), &facts(due + 1, &s, &[])));
    assert_eq!(entry(&st).window.check_in, CheckIn::Fired);
    assert_eq!(
        verdict(st.step(stop(), &facts(due + 2, &s, &[]))),
        Verdict::Park,
        "once"
    );
}

/// A check-in whose presentation fails, dies with its monitor, or
/// outlives its result's bound is still owed: the next park presents it
/// again, and only a delivery spends it.
#[test]
fn a_check_in_not_delivered_stays_owed() {
    let s = session();
    for ending in ["emit failed", "restart", "overdue"] {
        let mut st = standing(&s);
        let due = st.next_deadline().unwrap();
        let (p, _) = checked_in(verdict(st.step(stop(), &facts(due, &s, &[]))));
        let mut st = match ending {
            "emit failed" => {
                settled(st.step(Event::EmitFailed(p), &facts(due, &s, &[])));
                st
            }
            "restart" => restore(&serde_json::to_string(&st.persisted()).unwrap()),
            _ => {
                let late = facts(due + DELIVERY_RESULT_WITHIN, &s, &[]);
                settled(st.step(Event::ResultOverdue, &late));
                st
            }
        };
        assert_eq!(entry(&st).window.check_in, CheckIn::Due, "{ending}");
        let later = due + 2 * DELIVERY_RESULT_WITHIN;
        let (p, names) = checked_in(verdict(st.step(stop(), &facts(later, &s, &[]))));
        assert_eq!(names, [SUBAGENT_NAME], "{ending}");
        committed(st.step(Event::Delivered(p), &facts(later, &s, &[])));
        assert_eq!(entry(&st).window.check_in, CheckIn::Fired, "{ending}");
    }
}

/// The incident of 2026-11-11: six `clank run`s fall due at different
/// times while the agent's hook stays parked. Each is handed to that park
/// as soon as it falls due, and the park that follows waits for the next;
/// two due at one moment share one check-in.
#[test]
fn runs_falling_due_while_parked_are_each_checked_in_through_the_park() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    for (i, at) in [0, 1, 2, 3, 5, 5].into_iter().enumerate() {
        st.step(
            run(&format!("review {i}"), 7001 + i as i32),
            &facts(T0 + at * MINUTE, &s, &[]),
        );
    }
    assert_eq!(
        verdict(st.step(stop(), &facts(T0 + 6 * MINUTE, &s, &[]))),
        Verdict::Park
    );
    let mut shown = Vec::new();
    while let Some(due) = st.next_deadline() {
        assert_eq!(
            verdict(st.step(parked(), &facts(due - 1, &s, &[]))),
            Verdict::Park
        );
        assert!(effects(st.step(Event::CheckInDue, &facts(due, &s, &[]))).is_empty());
        let (p, names) = checked_in(verdict(st.step(parked(), &facts(due, &s, &[]))));
        committed(st.step(Event::Delivered(p), &facts(due, &s, &[])));
        shown.push(names);
        assert_eq!(
            verdict(st.step(stop(), &facts(due + 1, &s, &[]))),
            Verdict::Park,
            "the next park waits for the next"
        );
    }
    assert_eq!(
        shown,
        [
            vec!["review 0"],
            vec!["review 1"],
            vec!["review 2"],
            vec!["review 3"],
            vec!["review 4", "review 5"],
        ]
    );
    assert!(
        st.attendances
            .iter()
            .all(|e| e.window.check_in == CheckIn::Fired)
    );
}

/// Entries falling due while the agent works are owed and nothing
/// presents them: not a compaction in the turn a delivery started, and not
/// a session start. The next Stop carries
/// every one owed in one check-in.
#[test]
fn check_ins_falling_due_mid_turn_wait_for_the_next_park() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    st.step(declare("reviewer"), &facts(T0, &s, &[]));
    st.step(run("test run", 7001), &facts(T0 + MINUTE, &s, &[]));
    let (p, _) = deliver(&mut st, &facts(T0 + 2 * MINUTE, &s, &a));
    committed(st.step(Event::Delivered(p), &facts(T0 + 2 * MINUTE, &s, &a)));
    for due in [T0 + 10 * MINUTE, T0 + 11 * MINUTE] {
        assert!(effects(st.step(Event::CheckInDue, &facts(due, &s, &[]))).is_empty());
    }
    assert!(
        st.attendances
            .iter()
            .all(|e| e.window.check_in == CheckIn::Due)
    );
    let late = facts(T0 + 20 * MINUTE, &s, &[]);
    assert_eq!(
        verdict(st.step(compacted(), &late)),
        Verdict::Park,
        "no Stop stands: the turn is not over"
    );
    assert_eq!(
        verdict(st.step(Event::SessionStart(s.clone()), &late)),
        Verdict::Park
    );
    let (_, names) = checked_in(verdict(st.step(stop(), &late)));
    assert_eq!(names, ["reviewer", "test run"]);
}

/// Every harness is checked in on at its turn end: through the park of
/// every hook that parks, and through Grok's native Stop, beside the
/// request for its wait when it asks for one.
#[test]
fn a_check_in_rides_every_harness_turn_end() {
    let s = session();
    for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
        let mut st = standing(&s);
        let due = st.next_deadline().unwrap();
        let (_, names) = checked_in(verdict(
            st.step(Event::TurnEnd(report(tool)), &facts(due, &s, &[])),
        ));
        assert_eq!(names, [SUBAGENT_NAME], "{tool:?}");
    }
    for registered in [false, true] {
        let mut st = standing(&s);
        let due = st.next_deadline().unwrap();
        let v = verdict(st.step(
            native(&s, registered, TaskSnapshot::Unavailable),
            &facts(due, &s, &[]),
        ));
        let Verdict::Deliver { notices, .. } = &v else {
            panic!("{v:?}");
        };
        assert_eq!(
            notices.contains(&Notice::ArmWait),
            !registered,
            "only a Stop with no wait asks for one: {v:?}"
        );
        let (_, names) = checked_in(v);
        assert_eq!(names, [SUBAGENT_NAME], "registered {registered}");
    }
}

/// Grok's native Stop delivers its check-in with the same receipt as any
/// presentation: spent only by a delivery, left owed by a failed emit or a
/// result that never came, and presented again at the next native Stop. A
/// renewal while one is in flight keeps its own. With nothing owed, or for
/// another session, the Stop is answered as before.
#[test]
fn groks_native_stop_carries_its_check_in_with_a_receipt() {
    let s = session();
    let grok = |registered| native(&s, registered, TaskSnapshot::Unavailable);
    let mut st = standing(&s);
    assert_eq!(
        verdict(st.step(grok(true), &facts(T0 + MINUTE, &s, &[]))),
        Verdict::Silent(SilentWhy::WaitArmed),
        "nothing owed yet"
    );
    let due = st.next_deadline().unwrap();
    assert_eq!(
        verdict(st.step(
            native(&other_session(), true, TaskSnapshot::Unavailable),
            &facts(due, &s, &[])
        )),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    for ending in ["emit failed", "overdue"] {
        let (p, _) = checked_in(verdict(st.step(grok(true), &facts(due, &s, &[]))));
        assert_eq!(entry(&st).window.check_in, CheckIn::Due, "{ending}");
        match ending {
            "emit failed" => settled(st.step(Event::EmitFailed(p), &facts(due, &s, &[]))),
            _ => settled(st.step(
                Event::ResultOverdue,
                &facts(due + DELIVERY_RESULT_WITHIN, &s, &[]),
            )),
        };
        assert_eq!(entry(&st).window.check_in, CheckIn::Due, "{ending}");
    }
    let later = due + 2 * DELIVERY_RESULT_WITHIN;
    let (p, _) = checked_in(verdict(st.step(grok(true), &facts(later, &s, &[]))));
    committed(st.step(Event::Delivered(p), &facts(later, &s, &[])));
    assert_eq!(entry(&st).window.check_in, CheckIn::Fired);
    assert_eq!(
        verdict(st.step(grok(true), &facts(later + 1, &s, &[]))),
        Verdict::Silent(SilentWhy::WaitArmed),
        "once"
    );

    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let (p, _) = checked_in(verdict(st.step(grok(true), &facts(due, &s, &[]))));
    st.step(declare(SUBAGENT_NAME), &facts(due + 1, &s, &[]));
    committed(st.step(Event::Delivered(p), &facts(due + 2, &s, &[])));
    assert_eq!(
        entry(&st).window.check_in,
        CheckIn::Armed,
        "the renewal keeps its own"
    );
    let next = st.next_deadline().unwrap();
    let (_, names) = checked_in(verdict(st.step(grok(true), &facts(next, &s, &[]))));
    assert_eq!(names, [SUBAGENT_NAME]);
}

/// A wake carrying work carries the check-in owed beside it, and its one
/// delivery commits the work's receipts and spends the check-in.
#[test]
fn a_wake_carrying_work_carries_the_check_in_beside_it() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let v = verdict(st.step(stop(), &facts(due, &s, &a)));
    assert_eq!(check_in_of(&v).map(<[_]>::len), Some(1));
    let (p, identities) = presented(v);
    assert_eq!(identities, ["event:a"]);
    let fx = committed(st.step(Event::Delivered(p), &facts(due, &s, &a)));
    assert!(
        fx.contains(&Effect::CommitReceipts(vec!["read:a".into()])),
        "{fx:?}"
    );
    assert_eq!(entry(&st).window.check_in, CheckIn::Fired);
}

/// A run gone by its check-in is presented none: it has ended.
#[test]
fn a_run_gone_by_its_check_in_owes_none() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    let due = st.next_deadline().unwrap();
    assert_eq!(verdict(st.step(stop(), &dead(due, &s, &[]))), Verdict::Park);
    assert_eq!(st.next_deadline(), None, "never due again");
}

/// A renewal arms one more check-in, for its new expectation. Renewed
/// after a delivery, the next is presented at the new deadline; renewed
/// before one, the old window's is never presented; renewed while one is
/// in flight, that delivery spends only the entries still owed what it
/// carried, so the renewed one stays armed and is checked in on again.
#[test]
fn a_renewal_arms_a_new_check_in_and_no_old_presentation_spends_it() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let (p, _) = checked_in(verdict(st.step(stop(), &facts(due, &s, &[]))));
    committed(st.step(Event::Delivered(p), &facts(due, &s, &[])));
    st.step(declare(SUBAGENT_NAME), &facts(due + 1, &s, &[]));
    assert_eq!(entry(&st).window.check_in, CheckIn::Armed);
    let next = st.next_deadline().unwrap();
    assert_eq!(next, due + 1 + 10 * MINUTE);
    assert_eq!(
        verdict(st.step(stop(), &facts(next - 1, &s, &[]))),
        Verdict::Park
    );
    let (_, names) = checked_in(verdict(st.step(parked(), &facts(next, &s, &[]))));
    assert_eq!(names, [SUBAGENT_NAME], "renewed after: checked in again");

    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    st.step(Event::CheckInDue, &facts(due, &s, &[]));
    st.step(declare(SUBAGENT_NAME), &facts(due + 1, &s, &[]));
    assert_eq!(
        entry(&st).window.check_in,
        CheckIn::Armed,
        "renewed before: unfired"
    );
    assert_eq!(
        verdict(st.step(stop(), &facts(due + 2, &s, &[]))),
        Verdict::Park
    );

    let mut st = standing(&s);
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    let due = st.next_deadline().unwrap();
    let (p, names) = checked_in(verdict(st.step(stop(), &facts(due, &s, &[]))));
    assert_eq!(names, [SUBAGENT_NAME, "test run"]);
    st.step(declare(SUBAGENT_NAME), &facts(due + 1, &s, &[]));
    committed(st.step(Event::Delivered(p), &facts(due + 2, &s, &[])));
    assert_eq!(
        [
            &st.attendances[0].window.check_in,
            &st.attendances[1].window.check_in
        ],
        [&CheckIn::Armed, &CheckIn::Fired],
        "renewed in flight: the delivery spends only the run's"
    );
    let next = st.next_deadline().unwrap();
    assert_eq!(next, due + 1 + 10 * MINUTE);
    assert_eq!(
        verdict(st.step(stop(), &facts(next - 1, &s, &[]))),
        Verdict::Park
    );
    let (_, names) = checked_in(verdict(st.step(parked(), &facts(next, &s, &[]))));
    assert_eq!(names, [SUBAGENT_NAME]);
}

/// With no waiter nothing presents an owed check-in. It survives a monitor
/// restart, holds nothing off recovery past its deadline, and the next
/// park presents it once.
#[test]
fn an_owed_check_in_waits_for_a_park_across_a_restart_and_is_delivered_once() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    assert!(effects(st.step(Event::CheckInDue, &facts(due, &s, &[]))).is_empty());
    assert_eq!(st.recovery_quiet(&facts(due + 1, &s, &[])), None);
    let mut st = restore(&serde_json::to_string(&st.persisted()).unwrap());
    assert_eq!(entry(&st).window.check_in, CheckIn::Due);
    assert_eq!(st.next_deadline(), None, "nothing to wake the monitor for");
    let (p, names) = checked_in(verdict(st.step(stop(), &facts(due + MINUTE, &s, &[]))));
    assert_eq!(names, [SUBAGENT_NAME]);
    committed(st.step(Event::Delivered(p), &facts(due + MINUTE, &s, &[])));
    let mut st = restore(&serde_json::to_string(&st.persisted()).unwrap());
    assert_eq!(
        verdict(st.step(stop(), &facts(due + 2 * MINUTE, &s, &[]))),
        Verdict::Park,
        "delivered once"
    );
}

/// `clank attend <desc>` renews the run that `--desc` names, as its check-in
/// says; a clear by that name clears it.
#[test]
fn a_run_is_renewed_and_cleared_by_its_desc() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    st.step(declare("other"), &facts(T0, &s, &[]));
    let due = st.next_deadline().unwrap();
    st.step(Event::CheckInDue, &facts(due, &s, &[]));
    assert_eq!(
        attend_reply(st.step(declare("test run"), &facts(due + 1, &s, &[]))),
        AttendReply::Renewed
    );
    let renewed = &st.attendances[0];
    assert_eq!(renewed.proof(), Some(&proof_of(7001)));
    assert_eq!((renewed.started, renewed.window.since), (T0, due + 1));
    assert_eq!(renewed.window.check_in, CheckIn::Armed);
    assert_eq!(
        st.attendances.len(),
        2,
        "no declaration was added beside it"
    );
    assert_eq!(
        attend_reply(st.step(
            Event::Attend(AttendRequest::Clear {
                session: s.clone(),
                which: Clearing::Named("test run".into()),
            }),
            &facts(due + 2, &s, &[])
        )),
        AttendReply::Cleared { removed: 1 }
    );
    assert_eq!(names(&st), ["other"]);
}

/// A failed exec takes back exactly the run it registered, by its proof.
#[test]
fn a_failed_run_takes_back_only_its_own_entry() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    st.step(run("test run", 7002), &facts(T0, &s, &[]));
    st.step(declare(SUBAGENT_NAME), &facts(T0, &s, &[]));
    assert_eq!(
        attend_reply(st.step(
            Event::Attend(AttendRequest::Clear {
                session: s.clone(),
                which: Clearing::Run(proof_of(7002)),
            }),
            &facts(T0, &s, &[])
        )),
        AttendReply::Cleared { removed: 1 }
    );
    assert_eq!(names(&st), ["test run", SUBAGENT_NAME]);
    assert_eq!(st.attendances[0].proof(), Some(&proof_of(7001)));
}

#[test]
fn another_session_changes_nothing_and_a_new_binding_drops_the_old() {
    let s = session();
    let o = other_session();
    let mut st = standing(&s);
    let before = st.clone();
    let mut stale = report(Tool::Claude);
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
fn auto_off_a_question_and_a_block_come_before_attendance() {
    let s = session();
    let mut st = standing(&s);
    let mut off = facts(T0 + MINUTE, &s, &[]);
    off.auto = Ok(AutoMode::Off);
    assert_eq!(
        verdict(st.step(stop(), &off)),
        Verdict::Silent(SilentWhy::AutoOff)
    );
    let mut unreadable = facts(T0 + MINUTE, &s, &[]);
    unreadable.auto = Err("config".into());
    assert_eq!(
        verdict(st.step(stop(), &unreadable)),
        Verdict::Failed("config".into())
    );

    let mut asking = facts(T0 + MINUTE, &s, &[]);
    asking.asking = true;
    assert_eq!(st.recovery_quiet(&asking), Some(Quiet::Asking));
    let mut blocked = facts(T0 + MINUTE, &s, &[]);
    blocked.blocked = true;
    assert_eq!(
        st.recovery_quiet(&blocked),
        st.recovery_quiet(&facts(T0 + MINUTE, &s, &[])),
        "a block withholds review work from what is owed; it holds nothing else back"
    );
}

/// Recovery holds back only while an entry quiets everything owed, and
/// only before its deadline.
#[test]
fn recovery_is_quiet_only_while_attendance_quiets_everything_owed() {
    let s = session();
    let a = owed("event:a", "read:a");
    let st = received_then(&s, &a, declare(SUBAGENT_NAME));
    assert_eq!(
        st.recovery_quiet(&facts(T0 + 2 * MINUTE, &s, &a)),
        Some(Quiet::Attending)
    );
    assert_eq!(st.recovery_quiet(&facts(T0 + 10 * MINUTE, &s, &a)), None);
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    assert_eq!(
        st.recovery_quiet(&facts(T0 + 2 * MINUTE, &s, &ab)),
        None,
        "work never received stays recoverable"
    );
}

/// Past its deadline an entry quiets nothing, its check-in owed or not.
#[test]
fn received_work_is_quiet_until_the_deadline_even_while_its_check_in_is_owed() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, run("test run", 7001));
    let deadline = st.next_deadline().unwrap();
    let before = facts(deadline - 1, &s, &a);
    assert_eq!(st.recovery_quiet(&before), Some(Quiet::Attending));
    assert_eq!(
        verdict(st.step(Event::Parked(report(Tool::Claude)), &before)),
        Verdict::Park
    );
    let at = facts(deadline, &s, &a);
    st.step(Event::CheckInDue, &at);
    assert_eq!(entry(&st).window.check_in, CheckIn::Due);
    assert_eq!(st.recovery_quiet(&at), None);
    let (_, identities) = presented(verdict(st.step(Event::Parked(report(Tool::Claude)), &at)));
    assert_eq!(identities, ["event:a"]);
}

#[test]
fn clearing_presents_received_work_again() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, run("test run", 7001));
    st.step(clear_all(), &facts(T0 + MINUTE, &s, &a));
    let (_, identities) = presented(verdict(st.step(stop(), &facts(T0 + 2 * MINUTE, &s, &a))));
    assert_eq!(identities, ["event:a"]);

    let mut again = received_then(&s, &a, run("test run", 7001));
    again.step(clear_all(), &facts(T0 + MINUTE, &s, &a));
    again.step(run("test run", 7001), &facts(T0 + MINUTE, &s, &a));
    let (_, identities) = presented(verdict(again.step(stop(), &facts(T0 + 2 * MINUTE, &s, &a))));
    assert_eq!(
        identities,
        ["event:a"],
        "a cleared attendance's quiet is not inherited by the next"
    );
}

#[test]
fn work_whose_delivery_failed_or_was_lost_stays_deliverable() {
    let s = session();
    let a = owed("event:a", "read:a");
    let ab = [a[0].clone(), owed("event:b", "read:b")[0].clone()];
    let mut st = received_then(&s, &a, run("test run", 7001));

    let (failed, _) = presented(verdict(st.step(stop(), &facts(T0 + 2 * MINUTE, &s, &ab))));
    st.step(Event::EmitFailed(failed), &facts(T0 + 2 * MINUTE, &s, &ab));
    let (lost, identities) = presented(verdict(st.step(stop(), &facts(T0 + 3 * MINUTE, &s, &ab))));
    assert_eq!(identities, ["event:b"], "a failed emit received nothing");

    let restarted: Persisted =
        serde_json::from_str(&serde_json::to_string(&st.persisted()).unwrap()).unwrap();
    let mut st = SessionState::restored(restarted);
    let (_, identities) = presented(verdict(st.step(stop(), &facts(T0 + 4 * MINUTE, &s, &ab))));
    assert_eq!(identities, ["event:b"], "a lost result received nothing");
    assert!(matches!(
        settled(st.step(Event::Delivered(lost), &facts(T0 + 4 * MINUTE, &s, &ab))),
        ResultOutcome::ProtocolViolation(_)
    ));
    assert_eq!(st.received, ["event:a"]);
}

#[test]
fn a_session_start_presents_only_unreceived_work_while_attending() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, run("test run", 7001));
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
fn quieted_work_is_neither_presented_nor_counted() {
    let s = session();
    let a = owed("gate:a", "r");
    let mut st = SessionState::new(s.clone());
    let (p1, _) = deliver(&mut st, &facts(T0, &s, &a));
    st.step(Event::Delivered(p1), &facts(T0, &s, &a));
    assert_eq!(st.repeat.as_ref().map(|r| r.count), Some(1));
    st.step(run("test run", 7001), &facts(T0, &s, &a));
    assert_eq!(
        verdict(st.step(stop(), &facts(T0 + 1, &s, &a))),
        Verdict::Park
    );
    assert_eq!(st.repeat, None, "a quieted episode is over");
}

/// Grok's waiter evidence is the monitor's registration of it. A Stop
/// that continues the turn asked to arm one treats a wait not yet
/// registered as pending, so it is never asked for twice.
#[test]
fn a_native_handoff_asks_for_one_wait_through_the_drive_gate() {
    let s = session();
    let handoff = |wait_registered, continued| Handoff {
        session: s.clone(),
        wait_registered,
        continued,
        tasks: TaskSnapshot::Unavailable,
        hook_started: None,
    };
    let on = facts(T0, &s, &[]);
    assert_eq!(
        handoff_verdict(&handoff(false, false), &on),
        Verdict::Notify(vec![Notice::ArmWait]),
        "no registered waiter: arm one"
    );
    for (registered, continued) in [(true, false), (false, true), (true, true)] {
        assert_eq!(
            handoff_verdict(&handoff(registered, continued), &on),
            Verdict::Silent(SilentWhy::WaitArmed),
            "registered {registered}, continued {continued}"
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

mod old_bytes {
    pub const NONE: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":"None","repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":[],"unwatched":null}"#;
    pub const INTENT: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":{"Intent":{"selector":{"RunDesc":"test run"},"desc":"test run","since":1000000,"expect":600000,"shell":{"pid":7001,"token":"mac1:7001:1"}}},"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":[],"unwatched":null}"#;
    pub const OTHER_RENEWAL: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendance":{"Standing":{"task_id":"a25eee2eef81caf3e","kind":"Subagent","desc":"capture-probe sleeper","since":1000000,"expect":600000,"shell":null,"check_in":"Armed","renewal":{"selector":{"RunDesc":"test run"},"desc":"test run","since":1120000,"expect":1200000,"shell":{"pid":7001,"token":"mac1:7001:1"}}}},"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
    /// The set of entries resolved against task listings, as the last
    /// build before this one wrote it.
    /// Declarations as the build that ended them at the next turn start
    /// wrote them, one with its turn already ended.
    pub const TURN_SCOPED: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendances":[{"work":{"Declared":{"name":"capture-probe sleeper","seen_turn_end":true}},"desc":"capture-probe sleeper","started":1000000,"since":1000000,"expect":600000,"check_in":"Armed"},{"work":{"Declared":{"name":"reviewer","seen_turn_end":false}},"desc":"reviewer","started":1000000,"since":1000000,"expect":600000,"check_in":"Fired"}],"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
    /// Each check-in state a build that typed check-ins could save.
    pub const TYPED_CHECK_INS: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendances":[{"work":{"Declared":{"name":"lost"}},"desc":"lost","started":1000000,"since":1000000,"expect":600000,"check_in":{"Undeliverable":"its pane was not found"}},{"work":{"Run":{"pid":7001,"token":"mac1:7001:1"}},"desc":"gone","started":1000000,"since":1000000,"expect":600000,"check_in":"ProcessGone"},{"work":{"Declared":{"name":"held"}},"desc":"held","started":1000000,"since":1000000,"expect":600000,"check_in":{"Withheld":"Asking"}},{"work":{"Declared":{"name":"typed"}},"desc":"typed","started":1000000,"since":1000000,"expect":600000,"check_in":"Fired"},{"work":{"Declared":{"name":"armed"}},"desc":"armed","started":1000000,"since":1000000,"expect":1200000,"check_in":"Armed"}],"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":[],"unwatched":null}"#;
    pub const LISTED: &str = r#"{"binding":"00000000-0000-0000-0000-000000000000","attendances":[{"selector":{"Name":"capture-probe sleeper"},"resolved":{"task_id":"a25eee2eef81caf3e","kind":"Subagent"},"desc":"capture-probe sleeper","since":1000000,"expect":600000,"shell":null,"check_in":"Armed"},{"selector":{"RunDesc":"test run"},"resolved":null,"desc":"test run","since":1060000,"expect":600000,"shell":{"pid":7001,"token":"mac1:7001:1"},"check_in":"Unarmed"},{"selector":{"Name":"watcher"},"resolved":{"task_id":"m1","kind":{"Unknown":"monitor"}},"desc":null,"since":1000000,"expect":600000,"shell":null,"check_in":{"Withheld":"AutoOff"}}],"repeat":null,"outstanding":null,"outstanding_episode_broken":false,"resolved":null,"next_seq":3,"received":["event:a"],"unwatched":null}"#;
}

fn restore(bytes: &str) -> SessionState {
    SessionState::restored(serde_json::from_str(bytes).expect("old bytes load"))
}

/// State saved by every older build loads: a run keeps its registration,
/// so its end is still recorded, and a named attendance keeps the task a
/// listing resolved it to, so the next listing still ends it.
#[test]
fn every_older_saved_shape_loads_with_its_runs_registered() {
    let s = session();
    let window = |id, since, expect, check_in| Window {
        id,
        since,
        expect,
        check_in,
    };
    let bound = |name: &str, task: &str, since, expect, check_in, id| Entry {
        work: Attending::Declared {
            name: name.into(),
            task: Some(task.into()),
            kind: None,
            misses: 0,
            tracked: Tracked::default(),
        },
        desc: name.into(),
        started: since,
        window: window(id, since, expect, check_in),
    };
    let declared = |name: &str, since, expect, check_in, id| Entry {
        work: Attending::declared(name.into()),
        desc: name.into(),
        started: since,
        window: window(id, since, expect, check_in),
    };
    let test_run = |since, expect, id| Entry {
        work: Attending::Run(proof_of(7001)),
        desc: "test run".into(),
        started: since,
        window: window(id, since, expect, CheckIn::Armed),
    };
    assert!(restore(old_bytes::NONE).attendances.is_empty());
    assert_eq!(restore(old_bytes::NONE).next_seq, 3);
    assert_eq!(
        restore(old_bytes::INTENT).attendances,
        [test_run(T0, 10 * MINUTE, 1)]
    );
    assert_eq!(
        restore(old_bytes::OTHER_RENEWAL).attendances,
        [
            bound(
                SUBAGENT_NAME,
                SUBAGENT_ID,
                T0,
                10 * MINUTE,
                CheckIn::Armed,
                1
            ),
            test_run(T0 + 2 * MINUTE, 20 * MINUTE, 2),
        ]
    );
    let listed = restore(old_bytes::LISTED);
    assert_eq!(
        listed.attendances,
        [
            bound(
                SUBAGENT_NAME,
                SUBAGENT_ID,
                T0,
                10 * MINUTE,
                CheckIn::Armed,
                1
            ),
            test_run(T0 + MINUTE, 10 * MINUTE, 2),
            bound("watcher", "m1", T0, 10 * MINUTE, CheckIn::Due, 3),
        ]
    );
    let mut ended = listed.clone();
    assert!(ended.end_gone_runs(&DEAD));
    assert_eq!(ended.attendances.len(), 2, "only the run ends");
    assert_eq!(listed.next_window, 3, "a new window is not an old one");

    let mut st = restore(old_bytes::LISTED);
    st.step(stop(), &facts(T0 + 2 * MINUTE, &s, &[]));
    assert_eq!(names(&st).len(), 3, "a Stop without a listing ends nothing");
    st.step(
        listed_stop(&[(SUBAGENT_ID, SUBAGENT_NAME)]),
        &facts(T0 + 2 * MINUTE, &s, &[]),
    );
    assert_eq!(
        names(&st),
        [SUBAGENT_NAME, "test run"],
        "the listing ends the task it no longer has"
    );

    let turn_scoped = restore(old_bytes::TURN_SCOPED);
    assert_eq!(
        turn_scoped.attendances,
        [
            declared(SUBAGENT_NAME, T0, 10 * MINUTE, CheckIn::Armed, 1),
            declared("reviewer", T0, 10 * MINUTE, CheckIn::Fired, 2),
        ]
    );
    let mut st = turn_scoped.clone();
    st.step(stop(), &facts(T0 + MINUTE, &s, &[]));
    assert_eq!(
        st.attendances, turn_scoped.attendances,
        "no turn start ends them now"
    );

    let back: Persisted =
        serde_json::from_str(&serde_json::to_string(&listed.persisted()).unwrap()).unwrap();
    assert_eq!(
        back.attendances, listed.attendances,
        "the new shape round-trips"
    );
}

/// A check-in an older build left without reaching the model (lost to a
/// missing pane, withheld by a gate, or skipped for a gone run) loads as
/// owed, and the next park presents it; one it typed loads as spent.
#[test]
fn check_ins_saved_by_a_typing_build_load_as_owed_or_spent() {
    let s = session();
    let mut st = restore(old_bytes::TYPED_CHECK_INS);
    let states: Vec<&CheckIn> = st.attendances.iter().map(|e| &e.window.check_in).collect();
    assert_eq!(
        states,
        [
            &CheckIn::Due,
            &CheckIn::Due,
            &CheckIn::Due,
            &CheckIn::Fired,
            &CheckIn::Armed
        ]
    );
    assert_eq!(st.next_deadline(), Some(T0 + 20 * MINUTE));
    let (_, names) = checked_in(verdict(st.step(stop(), &facts(T0 + 11 * MINUTE, &s, &[]))));
    assert_eq!(names, ["lost", "gone", "held"]);
}

#[test]
fn a_hook_with_no_monitor_says_its_notice_only_to_a_session_clank_drives() {
    let s = session();
    let owed = [OwedItem {
        identity: "gate:plan:abc".into(),
        receipts: vec!["r1".into()],
    }];
    let notice = || Notice::MonitorOutage {
        why: "no binary".into(),
    };
    let mut asking = facts(T0, &s, &owed);
    asking.asking = true;
    let mut blocked = facts(T0, &s, &owed);
    blocked.blocked = true;
    for f in [facts(T0, &s, &owed), facts(T0, &s, &[]), asking, blocked] {
        assert_eq!(
            unmonitored_verdict(&s, &f, notice()),
            Verdict::Notify(vec![notice()])
        );
    }
    assert_eq!(
        unmonitored_verdict(&other_session(), &facts(T0, &s, &owed), notice()),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    let mut off = facts(T0, &s, &owed);
    off.auto = Ok(AutoMode::Off);
    assert_eq!(
        unmonitored_verdict(&s, &off, notice()),
        Verdict::Silent(SilentWhy::AutoOff)
    );
}

#[test]
fn repeated_deliveries_count_and_a_human_turn_resets() {
    let items = vec!["a".to_string()];
    let place = |turn: &str, human| TurnPlace {
        turn: Some(turn.into()),
        position: None,
        human_turn: human,
        read_to: None,
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
    let first = verdict(st.step(Event::TurnEnd(report(Tool::Claude)), &facts(T0, &s, &a)));
    let Verdict::Deliver { presentation, .. } = first else {
        panic!("{first:?}");
    };
    st.step(Event::TurnEnd(report(Tool::Claude)), &facts(T0 + 1, &s, &b));
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
    let first = verdict(st.step(Event::TurnEnd(report(Tool::Claude)), &facts(T0, &s, &a)));
    let Verdict::Deliver { presentation, .. } = first else {
        panic!("{first:?}");
    };
    st.step(Event::Delivered(presentation), &facts(T0, &s, &a));
    st.step(
        Event::TurnEnd(report(Tool::Claude)),
        &facts(T0 + 1, &s, &[]),
    );
    let again = verdict(st.step(Event::TurnEnd(report(Tool::Claude)), &facts(T0 + 2, &s, &a)));
    assert!(
        matches!(again, Verdict::Deliver { repeat: 1, .. }),
        "{again:?}"
    );
}

/// A check-in with no work beside it waits while clank may not wake the
/// agent for anything but work, or may not wake it at all, at a Stop and
/// at Grok's native Stop alike, and the turn end after the gate lifts
/// presents it.
#[test]
fn a_check_in_held_by_a_gate_waits_for_it_to_lift() {
    let s = session();
    type TurnEnd = fn(&SessionId) -> Event;
    let carriers: [(&str, TurnEnd); 2] = [
        ("Stop", |_| stop()),
        ("Grok's native Stop", |s| {
            native(s, true, TaskSnapshot::Unavailable)
        }),
    ];
    for (carrier, turn_end) in carriers {
        for gate in [
            "auto off",
            "asking",
            "blocked",
            "user command",
            "bad config",
            "bad role",
        ] {
            let mut st = standing(&s);
            let due = st.next_deadline().unwrap();
            let mut f = facts(due, &s, &[]);
            match gate {
                "auto off" => f.auto = Ok(AutoMode::Off),
                "asking" => f.asking = true,
                "blocked" => f.blocked = true,
                "user command" => f.user_command = true,
                "bad config" => f.auto = Err("bad config".into()),
                _ => f.role = Err("no role".into()),
            }
            let held = verdict(st.step(turn_end(&s), &f));
            assert_eq!(check_in_of(&held), None, "{carrier}, {gate}: {held:?}");
            assert_eq!(
                entry(&st).window.check_in,
                CheckIn::Due,
                "{carrier}, {gate}"
            );
            let lifted = facts(due + 1, &s, &[]);
            let (_, names) = checked_in(verdict(st.step(turn_end(&s), &lifted)));
            assert_eq!(names, [SUBAGENT_NAME], "{carrier}, {gate}");
        }
    }
}

/// A renewal made while a presentation is in flight, whose new expectation
/// has passed too before that presentation is delivered, keeps the new
/// window's check-in owed: the old delivery spends only the window it was
/// presented for.
#[test]
fn an_old_delivery_never_spends_a_renewed_window_already_due() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let (p, _) = checked_in(verdict(st.step(stop(), &facts(due, &s, &[]))));
    st.step(declare(SUBAGENT_NAME), &facts(due + 1, &s, &[]));
    let renewed = st.next_deadline().unwrap();
    st.step(Event::CheckInDue, &facts(renewed, &s, &[]));
    assert_eq!(entry(&st).window.check_in, CheckIn::Due);
    committed(st.step(Event::Delivered(p), &facts(renewed + 1, &s, &[])));
    assert_eq!(
        entry(&st).window.check_in,
        CheckIn::Due,
        "the old window's delivery leaves the new one owed"
    );
    let (_, names) = checked_in(verdict(st.step(stop(), &facts(renewed + 2, &s, &[]))));
    assert_eq!(names, [SUBAGENT_NAME]);
}

#[test]
fn work_owed_while_a_presentation_is_in_flight_waits_for_its_result() {
    let s = session();
    let a = owed("event:a", "read:a");
    let b = owed("event:b", "read:b");
    let mut st = SessionState::new(s.clone());
    let (pa, _) = deliver(&mut st, &facts(T0, &s, &a));
    let parked = report(Tool::Claude);
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

    let mut human = report(Tool::Claude);
    human.place = TurnPlace {
        turn: Some("later".into()),
        position: Some(99),
        human_turn: true,
        read_to: None,
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
            Event::TurnEnd(report(Tool::Claude)),
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
            verdict(st.step(Event::TurnEnd(report(Tool::Claude)), &facts(T0 + i, &s, &a))),
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
        let mut r = report(Tool::Claude);
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
        let mut r = report(Tool::Claude);
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
        Event::TurnEnd(report(Tool::Claude)),
        &facts(T0 + 2, &s, &[]),
    );
    assert!(
        st.repeat.is_none(),
        "the observed empty set ended the episode"
    );
    st.step(Event::TurnEnd(report(Tool::Claude)), &facts(T0 + 3, &s, &a));
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
    st.step(Event::TurnEnd(report(Tool::Claude)), &facts(T0 + 2, &s, &a));
    st.step(Event::Delivered(p2), &facts(T0 + 3, &s, &a));
    let (_, next) = deliver(&mut st, &facts(T0 + 4, &s, &a));
    assert_eq!(next, 3);
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

/// A Stop placed at or before one already counted, for other items, is a
/// capture overtaken by newer deliveries: it states no count and changes
/// none. The same items there are a duplicate, stating their count.
#[test]
fn an_overtaken_capture_states_no_count_and_changes_none() {
    let at = |position| TurnPlace {
        turn: None,
        position: Some(position),
        human_turn: false,
        read_to: None,
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
            read_to: None,
        };
        let RepeatDecision::Counted(next) = count_delivery(prev.as_ref(), &items, &place) else {
            panic!("{turn}");
        };
        stated.push(next.count);
        prev = Some(next);
    }
    assert_eq!(stated, [1, 2, 1, 2]);
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
    let stop = report(Tool::Claude);
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

/// Grok's `backgroundTasks`, as its native Stop sends them.
fn grok_listing(tasks: serde_json::Value) -> TaskSnapshot {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Stop {
        background_tasks: Option<Vec<BackgroundTask>>,
    }
    let stop: Stop = serde_json::from_value(serde_json::json!({"backgroundTasks": tasks})).unwrap();
    TaskSnapshot::listed(stop.background_tasks)
}

fn native(s: &SessionId, wait_registered: bool, tasks: TaskSnapshot) -> Event {
    Event::NativeTurnEnd(Handoff {
        session: s.clone(),
        wait_registered,
        continued: false,
        tasks,
        hook_started: later(),
    })
}

/// Grok's native Stop resolves and ends declarations by its own listing,
/// for its own session only, and its waiter verdict is the handoff's
/// alone: the listing never says whether a wait runs. A subagent gone
/// from a Stop that lists only the agent's own `clank wait` has finished:
/// that wait is never a subagent's work. A shell newer than the subagent
/// holds it, as work it may have started. A run outlives it.
#[test]
fn a_native_stop_ends_declarations_by_its_listing_and_keeps_its_verdict() {
    let s = session();
    let mut st = standing(&s);
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    let sub = serde_json::json!([{"id": "g1", "type": "subagent", "status": "running",
        "description": SUBAGENT_NAME, "agentType": "explore"}]);
    let wait = serde_json::json!([{"id": "w1", "type": "shell", "status": "running",
        "command": "clank wait"}]);
    let other = other_session();
    assert_eq!(
        verdict(st.step(
            native(&other, false, grok_listing(serde_json::json!([]))),
            &facts(T0, &s, &[])
        )),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    assert_eq!(
        bound_to(&st),
        [None, None],
        "another session's listing is not"
    );
    assert_eq!(
        verdict(st.step(
            native(&s, false, grok_listing(sub.clone())),
            &facts(T0, &s, &[])
        )),
        Verdict::Notify(vec![Notice::ArmWait])
    );
    assert_eq!(bound_to(&st), [Some("g1"), None]);
    assert_eq!(
        verdict(st.step(
            native(&s, false, grok_listing(wait.clone())),
            &facts(T0, &s, &[])
        )),
        Verdict::Notify(vec![Notice::ArmWait]),
        "a listed `clank wait` is not a registered waiter"
    );
    assert_eq!(
        names(&st),
        ["test run"],
        "Grok stopped listing it, and its own wait is not the subagent's"
    );
    assert_eq!(
        verdict(st.step(
            native(&s, true, TaskSnapshot::Unavailable),
            &facts(T0, &s, &[])
        )),
        Verdict::Silent(SilentWhy::WaitArmed)
    );

    let mut st = standing(&s);
    st.step(native(&s, false, grok_listing(sub)), &facts(T0, &s, &[]));
    let tests = serde_json::json!([{"id": "w1", "type": "shell", "status": "running",
        "command": "clank wait"}, {"id": "t1", "type": "shell", "status": "running",
        "command": "cargo test"}]);
    st.step(native(&s, false, grok_listing(tests)), &facts(T0, &s, &[]));
    assert_eq!(
        entry(&st).phase(),
        Some(&Phase::Parked {
            on: vec![HeldTask {
                id: "t1".into(),
                kind: Some("shell".into()),
                description: None,
            }],
            sure: false,
        }),
        "a shell newer than the subagent may be its work; the wait beside it is not"
    );

    let mut st = standing(&s);
    st.step(
        native(&s, true, grok_listing(wait.clone())),
        &facts(T0, &s, &[]),
    );
    let v = verdict(st.step(native(&s, true, grok_listing(wait)), &facts(T0, &s, &[])));
    assert!(
        matches!(&v, Verdict::Notify(n) if matches!(n.as_slice(), [Notice::NoMatch { .. }])),
        "a wait already armed still reports the agent's own declaration: {v:?}"
    );
}

/// A run whose process cannot be seen has not ended: it keeps its
/// registration and owes no check-in as gone, but quiets nothing while
/// unseen.
#[test]
fn an_unseen_run_is_kept_and_never_ended() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, run("test run", 7001));
    assert!(!st.end_gone_runs(&UNSEEN));
    let unseen = Facts {
        liveness: &UNSEEN,
        ..facts(T0 + MINUTE, &s, &a)
    };
    assert_eq!(st.recovery_quiet(&unseen), None);
    let due = st.next_deadline().unwrap();
    let (_, checked) = checked_in(verdict(st.step(
        stop(),
        &Facts {
            liveness: &UNSEEN,
            ..facts(due, &s, &a)
        },
    )));
    assert_eq!(
        checked,
        ["test run"],
        "it may still be running: checked in on"
    );
    assert_eq!(names(&st), ["test run"]);
    assert!(st.end_gone_runs(&DEAD), "once seen gone, it ends");
    assert!(st.attendances.is_empty());
}

/// A presentation is in flight only for [`DELIVERY_RESULT_WITHIN`]. A
/// carrier that has not answered by then failed it, terminally: the work
/// stays owed and unreceived, the next Stop presents it afresh, and the
/// carrier's late success finds nothing in flight to commit.
#[test]
fn a_presentation_nobody_answers_for_fails_at_its_bound() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = SessionState::new(s.clone());
    assert_eq!(st.delivery_deadline(), None);
    let (stuck, _) = deliver(&mut st, &facts(T0, &s, &a));
    assert_eq!(st.delivery_deadline(), Some(T0 + DELIVERY_RESULT_WITHIN));
    assert_eq!(
        verdict(st.step(Event::TurnEnd(report(Tool::Claude)), &facts(T0 + 1, &s, &a))),
        Verdict::Park,
        "one presentation at a time, while it is in flight"
    );

    let early = facts(T0 + DELIVERY_RESULT_WITHIN - 1, &s, &a);
    assert_eq!(
        settled(st.step(Event::ResultOverdue, &early)),
        ResultOutcome::InTime
    );
    assert!(st.outstanding.is_some(), "still in flight inside its bound");

    let at = facts(T0 + DELIVERY_RESULT_WITHIN, &s, &a);
    assert_eq!(
        settled(st.step(Event::ResultOverdue, &at)),
        ResultOutcome::Resolved(Vec::new()),
        "failed: nothing is committed"
    );
    assert_eq!(st.outstanding, None);
    assert_eq!(st.delivery_deadline(), None);
    assert!(st.received.is_empty(), "a silent carrier delivered nothing");
    assert_eq!(st.resolved, Some((stuck.clone(), Resolution::Failed)));
    assert_eq!(
        settled(st.step(Event::ResultOverdue, &at)),
        ResultOutcome::InTime,
        "nothing left to fail"
    );

    assert_eq!(
        settled(st.step(Event::Delivered(stuck.clone()), &at)),
        ResultOutcome::ProtocolViolation(stuck.clone()),
        "its late success commits nothing"
    );
    assert!(st.received.is_empty());
    let (again, _) = deliver(&mut st, &facts(T0 + DELIVERY_RESULT_WITHIN + 5, &s, &a));
    assert_ne!(again, stuck, "the work is presented afresh");
    assert_eq!(
        st.delivery_deadline(),
        Some(T0 + 2 * DELIVERY_RESULT_WITHIN + 5),
        "on a bound of its own"
    );
}

/// A presentation saved before a result had a bound carries no hand-over
/// time. It reads as handed at the epoch, so it is long overdue; and a
/// restart fails whatever was in flight anyway, since its carrier's
/// connection died with the monitor that held it.
#[test]
fn a_presentation_saved_without_a_hand_over_time_still_loads() {
    let old = r#"{"id":{"session":"00000000-0000-4000-8000-000000000001","seq":7},"identities":["event:a"],"receipts":["read:a"],"repeat":"Uncounted"}"#;
    let p: Presentation = serde_json::from_str(old).unwrap();
    assert_eq!(p.handed, 0);
    assert_eq!(p.expires(), DELIVERY_RESULT_WITHIN);
    let stored = format!(
        r#"{{"binding":"00000000-0000-4000-8000-000000000001","attendances":[],"repeat":null,"outstanding":{old},"outstanding_episode_broken":false,"resolved":null,"next_seq":7,"received":[],"unwatched":null}}"#
    );
    let persisted: Persisted = serde_json::from_str(&stored).unwrap();
    let st = SessionState::restored(persisted);
    assert_eq!(st.outstanding, None);
    assert_eq!(st.resolved.map(|(_, how)| how), Some(Resolution::Failed));
}

/// A Stop placed at `turn`'s boundary `position`.
fn placed_stop(turn: &str, position: u64) -> TurnEndReport {
    TurnEndReport {
        place: TurnPlace {
            turn: Some(turn.into()),
            position: Some(position),
            human_turn: false,
            read_to: None,
        },
        hook_started: None,
        ..report(Tool::Codex)
    }
}

fn stopped_at(turn: &str, position: u64, at: Ms) -> Option<Stopped> {
    Some(Stopped {
        turn: Some(turn.into()),
        position: Some(position),
        read_to: None,
        at,
        hook_started: None,
        listed: None,
    })
}

/// A Stop of `turn` whose boundary the adapter could not read.
fn unplaced_stop(turn: Option<&str>) -> TurnEndReport {
    TurnEndReport {
        place: TurnPlace {
            turn: turn.map(str::to_string),
            position: None,
            human_turn: false,
            read_to: None,
        },
        hook_started: None,
        ..report(Tool::Claude)
    }
}

fn unplaced_at(turn: Option<&str>, at: Ms) -> Option<Stopped> {
    Some(Stopped {
        turn: turn.map(str::to_string),
        position: None,
        read_to: None,
        at,
        hook_started: None,
        listed: None,
    })
}

/// Only a turn-end report is evidence that a turn is over, and it is
/// kept against the place the hook read it at. A park stepped again, a
/// compaction arriving as one, a session start and a Stop from a session
/// that is not bound record nothing. A Stop the adapter could not place
/// is kept as that: it replaces what the last one said and is never
/// dropped, since a turn did end.
#[test]
fn only_a_turn_end_report_records_a_stop() {
    let s = session();
    let none: [OwedItem; 0] = [];
    let mut st = SessionState::new(s.clone());
    for not_a_stop in [
        Event::Parked(placed_stop("t1", 40)),
        Event::SessionStart(s.clone()),
        Event::Observed,
        Event::CheckInDue,
    ] {
        st.step(not_a_stop.clone(), &facts(T0, &s, &none));
        assert_eq!(st.stopped, None, "{not_a_stop:?}");
    }
    let elsewhere = TurnEndReport {
        session: other_session(),
        ..placed_stop("t1", 40)
    };
    assert_eq!(
        verdict(st.step(Event::TurnEnd(elsewhere), &facts(T0, &s, &none))),
        Verdict::Silent(SilentWhy::StaleBinding)
    );
    assert_eq!(st.stopped, None, "another session's Stop");

    assert_eq!(
        verdict(st.step(Event::TurnEnd(placed_stop("t1", 40)), &facts(T0, &s, &none))),
        Verdict::Park
    );
    assert_eq!(st.stopped, stopped_at("t1", 40, T0));
    st.step(
        Event::TurnEnd(placed_stop("t1", 40)),
        &facts(T0 + MINUTE, &s, &none),
    );
    assert_eq!(
        st.stopped,
        stopped_at("t1", 40, T0),
        "the same Stop reported again keeps when it was first reported"
    );
    st.step(
        Event::TurnEnd(placed_stop("t1", 90)),
        &facts(T0 + 2 * MINUTE, &s, &none),
    );
    assert_eq!(
        st.stopped,
        stopped_at("t1", 90, T0 + 2 * MINUTE),
        "a later Stop of the same turn is another"
    );
    st.step(
        Event::TurnEnd(unplaced_stop(Some("t1"))),
        &facts(T0 + 3 * MINUTE, &s, &none),
    );
    assert_eq!(
        st.stopped,
        unplaced_at(Some("t1"), T0 + 3 * MINUTE),
        "a Stop with no place replaces the precise one and is kept"
    );
    st.step(
        Event::TurnEnd(unplaced_stop(Some("t1"))),
        &facts(T0 + 4 * MINUTE, &s, &none),
    );
    assert_eq!(
        st.stopped,
        unplaced_at(Some("t1"), T0 + 3 * MINUTE),
        "reported again for the turn it named, it keeps its time"
    );
    let nameless = || Event::TurnEnd(unplaced_stop(None));
    st.step(nameless(), &facts(T0 + 5 * MINUTE, &s, &none));
    assert_eq!(st.stopped, unplaced_at(None, T0 + 5 * MINUTE));
    st.step(nameless(), &facts(T0 + 6 * MINUTE, &s, &none));
    assert_eq!(
        st.stopped,
        unplaced_at(None, T0 + 6 * MINUTE),
        "naming no turn, it cannot be told from a later Stop and is dated afresh"
    );

    let seen = |len| TurnEndReport {
        place: TurnPlace {
            read_to: Some(ReadTo { file: 7, len }),
            ..TurnPlace::default()
        },
        ..report(Tool::Claude)
    };
    st.step(
        Event::TurnEnd(seen(400)),
        &facts(T0 + 7 * MINUTE, &s, &none),
    );
    st.step(
        Event::TurnEnd(seen(400)),
        &facts(T0 + 8 * MINUTE, &s, &none),
    );
    let kept = st.stopped.clone().unwrap();
    assert_eq!(
        (kept.read_to, kept.at),
        (Some(ReadTo { file: 7, len: 400 }), T0 + 7 * MINUTE),
        "the same report replayed, known by what its hook saw, is the same Stop"
    );
    st.step(
        Event::TurnEnd(seen(900)),
        &facts(T0 + 9 * MINUTE, &s, &none),
    );
    let later = st.stopped.clone().unwrap();
    assert_eq!(
        (later.read_to, later.at),
        (Some(ReadTo { file: 7, len: 900 }), T0 + 9 * MINUTE),
        "a hook that saw more of the record reports another Stop"
    );

    let off = Facts {
        auto: Ok(AutoMode::Off),
        ..facts(T0, &s, &none)
    };
    assert_eq!(
        verdict(st.step(Event::TurnEnd(placed_stop("t2", 10)), &off)),
        Verdict::Silent(SilentWhy::AutoOff)
    );
    assert_eq!(
        st.stopped,
        stopped_at("t2", 10, T0),
        "a turn clank does not drive still ended"
    );
}

/// The Stop stands until clank wakes the agent past it: a delivery that
/// reached it, or a notice. A delivery that failed woke nothing, and a
/// new binding owns none of the old one's evidence. It survives the
/// file.
#[test]
fn a_stop_stands_until_the_agent_is_woken_past_it() {
    let s = session();
    let a = owed("event:a", "read:a");
    let none: [OwedItem; 0] = [];

    let mut st = SessionState::new(s.clone());
    let Verdict::Deliver { presentation, .. } =
        verdict(st.step(Event::TurnEnd(placed_stop("t1", 40)), &facts(T0, &s, &a)))
    else {
        panic!("owed work is delivered");
    };
    assert_eq!(st.stopped, stopped_at("t1", 40, T0), "in flight");
    st.step(Event::EmitFailed(presentation), &facts(T0 + 1, &s, &a));
    assert_eq!(
        st.stopped,
        stopped_at("t1", 40, T0),
        "a delivery that failed woke nothing"
    );
    let restored = SessionState::restored(
        serde_json::from_str(&serde_json::to_string(&st.persisted()).unwrap()).unwrap(),
    );
    assert_eq!(restored.stopped, st.stopped, "kept across a restart");

    let Verdict::Deliver { presentation, .. } =
        verdict(st.step(Event::Parked(placed_stop("t1", 40)), &facts(T0 + 2, &s, &a)))
    else {
        panic!("presented afresh");
    };
    committed(st.step(Event::Delivered(presentation), &facts(T0 + 3, &s, &a)));
    assert_eq!(st.stopped, None, "delivered: the agent starts a turn");

    let mut st = SessionState::new(s.clone());
    st.feedback.push(Notice::ArmWait);
    assert!(matches!(
        verdict(st.step(Event::TurnEnd(placed_stop("t1", 40)), &facts(T0, &s, &none))),
        Verdict::Notify(_)
    ));
    assert_eq!(
        st.stopped, None,
        "a notice wakes it with nothing to wait for"
    );

    let mut st = SessionState::new(s.clone());
    st.step(Event::TurnEnd(placed_stop("t1", 40)), &facts(T0, &s, &none));
    st.rebind(other_session());
    assert_eq!(st.stopped, None, "another binding's turn");
}

/// Turn state saved before Stops were kept has none.
#[test]
fn state_saved_before_stops_were_kept_reports_none() {
    for old in [
        old_bytes::OTHER_RENEWAL,
        old_bytes::TURN_SCOPED,
        old_bytes::LISTED,
    ] {
        assert_eq!(restore(old).stopped, None);
    }
}

/// A person starts a turn with no clank presentation, so the Stop of the
/// turn before stays recorded. A compaction in the new turn is stepped as
/// a park, but its record has moved past that Stop: it carries no
/// check-in, which stays owed for the next Stop, while a park still at the
/// recorded Stop is handed it.
#[test]
fn review_old_stop_cannot_authorize_a_check_in_at_a_new_turns_compaction() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(declare("reviewer"), &facts(T0, &s, &[]));
    assert_eq!(
        verdict(st.step(Event::TurnEnd(placed_stop("t1", 40)), &facts(T0, &s, &[]))),
        Verdict::Park
    );
    let due = st.next_deadline().unwrap();
    let compacting = placed_stop("t2", 200);
    assert!(!st.stopped.as_ref().unwrap().stands_in(&compacting.place));
    let v = verdict(st.step(Event::Compacted(compacting), &facts(due, &s, &[])));
    assert!(
        check_in_of(&v).is_none(),
        "a Stop from t1 does not show t2 is over: {v:?}"
    );
    assert_eq!(entry(&st).window.check_in, CheckIn::Due);
    let (p, names) = checked_in(verdict(
        st.step(Event::Parked(placed_stop("t1", 40)), &facts(due, &s, &[])),
    ));
    assert_eq!(
        names,
        ["reviewer"],
        "the park at t1's own Stop still is one"
    );
    settled(st.step(Event::EmitFailed(p), &facts(due, &s, &[])));
    let (_, names) = checked_in(verdict(st.step(
        Event::TurnEnd(placed_stop("t2", 220)),
        &facts(due + 1, &s, &[]),
    )));
    assert_eq!(names, ["reviewer"], "t2's own Stop delivers it");
}

#[test]
fn review_empty_placement_is_not_identity_of_the_parked_stop() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(declare("reviewer"), &facts(T0, &s, &[]));
    let mut old_stop = report(Tool::Claude);
    old_stop.place = TurnPlace::default();
    old_stop.hook_started = Some(T0);
    assert_eq!(
        verdict(st.step(Event::TurnEnd(old_stop), &facts(T0, &s, &[]))),
        Verdict::Park
    );
    let due = st.next_deadline().unwrap();
    let mut later_compaction = report(Tool::Claude);
    later_compaction.place = TurnPlace::default();
    later_compaction.hook_started = Some(due);
    let v = verdict(st.step(Event::Compacted(later_compaction), &facts(due, &s, &[])));
    assert!(
        check_in_of(&v).is_none(),
        "equal missing placement is not evidence this compaction holds the old Stop: {v:?}"
    );
}

/// A report other than the one that recorded the Stop holds it only by
/// placement both sides read and that has not moved. A compaction whose
/// record could not be read, one naming the Stop's turn with no boundary,
/// or a Stop that itself was placed by name alone, is no such evidence:
/// the check-in stays owed for the next Stop.
#[test]
fn a_park_without_placement_evidence_carries_no_check_in() {
    let s = session();
    let named = |turn: &str| TurnPlace {
        turn: Some(turn.into()),
        ..TurnPlace::default()
    };
    let read = |len| TurnPlace {
        read_to: Some(ReadTo { file: 7, len }),
        ..TurnPlace::default()
    };
    for (case, stopped, compacting) in [
        ("unreadable compaction", read(100), TurnPlace::default()),
        ("unread Stop", TurnPlace::default(), read(100)),
        ("named but unplaced", named("t1"), named("t1")),
    ] {
        let mut st = standing(&s);
        let due = st.next_deadline().unwrap();
        let first = TurnEndReport {
            place: stopped,
            ..report(Tool::Claude)
        };
        st.step(Event::TurnEnd(first), &facts(due - 1, &s, &[]));
        let compaction = TurnEndReport {
            place: compacting,
            ..report(Tool::Claude)
        };
        let v = verdict(st.step(Event::Compacted(compaction), &facts(due, &s, &[])));
        assert_eq!(check_in_of(&v), None, "{case}: {v:?}");
        assert_eq!(entry(&st).window.check_in, CheckIn::Due, "{case}");
        let (_, names) = checked_in(verdict(st.step(stop(), &facts(due + 1, &s, &[]))));
        assert_eq!(names, [SUBAGENT_NAME], "{case}: the next Stop delivers it");
    }
}

/// A parked Stop that reads no record, as OpenCode's, or whose hook from
/// an older build gave no start time, is re-served at its check-in's
/// deadline as the very report the park holds, and is handed the check-in.
#[test]
fn a_re_served_unplaced_park_is_handed_its_check_in() {
    let s = session();
    let st = standing(&s);
    let due = st.next_deadline().unwrap();
    for hook_started in [later(), None] {
        let mut st = st.clone();
        let held = TurnEndReport {
            hook_started,
            ..report(Tool::OpenCode)
        };
        assert_eq!(
            verdict(st.step(Event::TurnEnd(held.clone()), &facts(due - 1, &s, &[]))),
            Verdict::Park
        );
        let (_, names) = checked_in(verdict(st.step(Event::Parked(held), &facts(due, &s, &[]))));
        assert_eq!(names, [SUBAGENT_NAME], "{hook_started:?}");
    }
}

/// Equal clock readings are not identity: a compaction at a later turn
/// whose hook clock reads the same as the old Stop's carries nothing.
#[test]
fn review_equal_hook_clock_cannot_override_a_different_turn() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(declare("reviewer"), &facts(T0, &s, &[]));
    let mut old = placed_stop("t1", 40);
    old.tool = Tool::Claude;
    old.hook_started = Some(T0);
    assert_eq!(
        verdict(st.step(Event::TurnEnd(old), &facts(T0, &s, &[]))),
        Verdict::Park
    );
    let due = st.next_deadline().unwrap();
    let mut compacted = placed_stop("t2", 200);
    compacted.tool = Tool::Claude;
    compacted.hook_started = Some(T0);
    assert!(!st.stopped.as_ref().unwrap().stands_in(&compacted.place));
    let v = verdict(st.step(Event::Compacted(compacted), &facts(due, &s, &[])));
    assert!(
        check_in_of(&v).is_none(),
        "a matching clock cannot turn t2 into the old t1 Stop: {v:?}"
    );
}

/// The listing that binds a pending declaration to its subagent, from the
/// subagent's own stop.
fn binding_report(s: &SessionId, at: Ms) -> ChildReport {
    ChildReport {
        session: s.clone(),
        child: "other-child".into(),
        hook_started: Some(at),
        seen: ChildSeen::Stopped {
            tasks: listing(&[(SUBAGENT_ID, SUBAGENT_NAME)]),
            agent: TurnPlace::default(),
        },
    }
}

#[test]
fn review_binding_a_declaration_does_not_change_its_check_in_window() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let (p, _) = checked_in(verdict(st.step(stop(), &facts(due, &s, &[]))));
    assert!(matches!(entry(&st).key(), EntryKey::Name(_)));
    assert!(st.child(&binding_report(&s, due + 1), &facts(due + 1, &s, &[])));
    assert_eq!(entry(&st).task(), Some(SUBAGENT_ID));
    let window = entry(&st).window.since;
    committed(st.step(Event::Delivered(p), &facts(due + 2, &s, &[])));
    let v = verdict(st.step(stop(), &facts(due + 3, &s, &[])));
    assert!(
        check_in_of(&v).is_none(),
        "binding renewed nothing at since={window}, but the delivered check-in is repeated: {v:?}"
    );
}

/// A presentation in flight across a binding is saved under the bound key.
/// Restored, it died with its monitor, so its check-in is presented once
/// more, and that delivery spends the bound entry's window.
#[test]
fn a_check_in_in_flight_across_a_binding_survives_a_restart_bound() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    let (lost, _) = checked_in(verdict(st.step(stop(), &facts(due, &s, &[]))));
    assert!(st.child(&binding_report(&s, due + 1), &facts(due + 1, &s, &[])));
    let saved: Persisted =
        serde_json::from_str(&serde_json::to_string(&st.persisted()).unwrap()).unwrap();
    assert_eq!(
        saved.outstanding.as_ref().map(|p| p.check_ins.clone()),
        Some(vec![CheckInRef {
            window: entry(&st).window.id,
        }])
    );
    let mut st = SessionState::restored(saved);
    assert!(matches!(
        settled(st.step(Event::Delivered(lost), &facts(due + 2, &s, &[]))),
        ResultOutcome::ProtocolViolation(_)
    ));
    assert_eq!(entry(&st).window.check_in, CheckIn::Due);
    let (p, names) = checked_in(verdict(st.step(stop(), &facts(due + 3, &s, &[]))));
    assert_eq!(names, [SUBAGENT_NAME]);
    committed(st.step(Event::Delivered(p), &facts(due + 4, &s, &[])));
    assert_eq!(entry(&st).window.check_in, CheckIn::Fired);
}

/// A declaration renewed while the first is bound to its subagent, its
/// window presented, then merged into the bound one by a later listing.
/// Returns the state just before the merge, the presentation carrying the
/// pending window, and that window's deadline.
fn presented_then_pending(s: &SessionId) -> (SessionState, PresentationId, Ms) {
    let mut st = standing(s);
    assert!(st.child(&binding_report(s, T0 + 1), &facts(T0 + 1, s, &[])));
    let old_due = st.next_deadline().unwrap();
    let (old, _) = checked_in(verdict(st.step(stop(), &facts(old_due, s, &[]))));
    committed(st.step(Event::Delivered(old), &facts(old_due, s, &[])));
    st.step(declare(SUBAGENT_NAME), &facts(old_due + 1, s, &[]));
    assert_eq!(st.attendances.len(), 2);
    let due = st.next_deadline().unwrap();
    let parked_report = TurnEndReport {
        tasks: TaskSnapshot::Known(vec![BackgroundTask::from(serde_json::json!({
            "id":"work", "type":"shell", "status":"running", "description":"long work"
        }))]),
        hook_started: Some(old_due + 2),
        ..report(Tool::Claude)
    };
    assert_eq!(
        verdict(st.step(
            Event::TurnEnd(parked_report.clone()),
            &facts(old_due + 2, s, &[])
        )),
        Verdict::Park
    );
    let (p, names) = checked_in(verdict(
        st.step(Event::Parked(parked_report), &facts(due, s, &[])),
    ));
    assert_eq!(names, [SUBAGENT_NAME]);
    (st, p, due)
}

#[test]
fn review_merging_a_pending_window_preserves_its_check_in_delivery() {
    let s = session();
    for delivered_before_merge in [false, true] {
        let (mut st, p, due) = presented_then_pending(&s);
        let window = st.attendances[1].window.clone();
        if delivered_before_merge {
            committed(st.step(Event::Delivered(p.clone()), &facts(due + 1, &s, &[])));
        }
        assert!(st.child(&binding_report(&s, due + 2), &facts(due + 2, &s, &[])));
        assert_eq!(st.attendances.len(), 1);
        assert_eq!(
            (entry(&st).window.id, entry(&st).window.since),
            (window.id, window.since),
            "the merge moves the pending window"
        );
        if !delivered_before_merge {
            committed(st.step(Event::Delivered(p), &facts(due + 3, &s, &[])));
        }
        assert_eq!(entry(&st).window.check_in, CheckIn::Fired);
        let v = verdict(st.step(stop(), &facts(due + 4, &s, &[])));
        assert!(
            check_in_of(&v).is_none(),
            "delivered before merge={delivered_before_merge}: {v:?}"
        );
    }
}

/// A merge while the pending window's presentation is in flight, then a
/// restart: the presentation died with its monitor, so the moved window is
/// still owed, presented once more, and spent by that delivery.
#[test]
fn a_window_merged_in_flight_survives_a_restart_owed() {
    let s = session();
    let (mut st, lost, due) = presented_then_pending(&s);
    assert!(st.child(&binding_report(&s, due + 1), &facts(due + 1, &s, &[])));
    let mut st = restore(&serde_json::to_string(&st.persisted()).unwrap());
    assert!(matches!(
        settled(st.step(Event::Delivered(lost), &facts(due + 2, &s, &[]))),
        ResultOutcome::ProtocolViolation(_)
    ));
    assert_eq!(entry(&st).window.check_in, CheckIn::Due);
    let (p, names) = checked_in(verdict(st.step(stop(), &facts(due + 3, &s, &[]))));
    assert_eq!(names, [SUBAGENT_NAME]);
    committed(st.step(Event::Delivered(p), &facts(due + 4, &s, &[])));
    assert_eq!(entry(&st).window.check_in, CheckIn::Fired);
    assert!(check_in_of(&verdict(st.step(stop(), &facts(due + 5, &s, &[])))).is_none());
}
