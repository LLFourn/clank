use super::*;

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

fn report(tool: Tool) -> TurnEndReport {
    TurnEndReport {
        tool,
        session: session(),
        place: TurnPlace::default(),
        tasks: TaskSnapshot::Unavailable,
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

fn check_ins(fx: Vec<Effect>) -> Vec<CheckInRef> {
    match fx.as_slice() {
        [Effect::CheckIn(lines)] => lines.iter().map(|l| l.check_in.clone()).collect(),
        other => panic!("not one check-in: {other:?}"),
    }
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
        assert_eq!(entry(&st).check_in, CheckIn::Armed);
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
    assert_eq!(entry(&st).since, T0 + MINUTE, "on the renewal's clock");
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
    assert_eq!(
        check_ins(effects(st.step(Event::CheckInDue, &facts(due, &s, &ab)))).len(),
        1
    );
    assert!(effects(st.step(Event::CheckInDue, &facts(due + MINUTE, &s, &ab))).is_empty());
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
/// under its pid; either way exactly one terminal record, and only once it
/// is written does the entry go. Nothing about it comes from a listing.
#[test]
fn a_gone_run_is_ended_once_and_only_after_its_record() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, run("test run", 7001));
    assert!(st.ended_runs(&LIVE, T0 + MINUTE).is_empty());
    let ended = st.ended_runs(&DEAD, T0 + 2 * MINUTE);
    assert_eq!(
        ended,
        [RunEnd {
            proof: proof_of(7001),
            desc: "test run".into(),
            started: T0,
            ended_seen: T0 + 2 * MINUTE,
        }]
    );
    assert_eq!(
        st.ended_runs(&DEAD, T0 + 3 * MINUTE).len(),
        1,
        "asking again before the record is written names it again"
    );
    assert_eq!(
        st.recovery_quiet(&dead(T0 + 2 * MINUTE, &s, &a)),
        None,
        "a gone run quiets nothing while its record is written"
    );
    st.end_run(&ended[0].proof);
    assert!(st.attendances.is_empty());
    assert!(
        st.ended_runs(&DEAD, T0 + 4 * MINUTE).is_empty(),
        "never twice"
    );
    assert!(st.received.is_empty(), "what it quieted is released");
}

/// A run's registration is durable: a monitor that restarts finds it and
/// ends it once, and a second restart finds nothing.
#[test]
fn a_restart_ends_a_run_that_went_while_nobody_watched_and_only_once() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    let saved = serde_json::to_string(&st.persisted()).unwrap();
    let mut back = SessionState::restored(serde_json::from_str(&saved).unwrap());
    let ended = back.ended_runs(&DEAD, T0 + MINUTE);
    assert_eq!(ended.len(), 1);
    back.end_run(&ended[0].proof);
    let saved = serde_json::to_string(&back.persisted()).unwrap();
    let again = SessionState::restored(serde_json::from_str(&saved).unwrap());
    assert!(again.ended_runs(&DEAD, T0 + 2 * MINUTE).is_empty());
}

/// Killing the command is its end like any other: the same one record.
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
    let ended = st.ended_runs(&Killed, T0 + MINUTE);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].desc, "server");
    st.end_run(&ended[0].proof);
    assert_eq!(names(&st), ["build"], "the other run is untouched");
}

/// A declaration is checked in on once at `--expect` if it still stands.
#[test]
fn a_declaration_is_checked_in_on_once_at_its_expectation() {
    let s = session();
    let mut st = standing(&s);
    let due = st.next_deadline().unwrap();
    assert_eq!(due, T0 + 10 * MINUTE);
    assert!(effects(st.step(Event::CheckInDue, &facts(due - 1, &s, &[]))).is_empty());
    let fired = effects(st.step(Event::CheckInDue, &facts(due, &s, &[])));
    let [Effect::CheckIn(lines)] = fired.as_slice() else {
        panic!("{fired:?}");
    };
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].name, SUBAGENT_NAME);
    assert!(!lines[0].run);
    assert!(effects(st.step(Event::CheckInDue, &facts(due + 1, &s, &[]))).is_empty());
    assert_eq!(st.next_deadline(), None);
}

/// A run due for its check-in after its process went owes no check-in:
/// its end is the wake.
#[test]
fn a_run_gone_by_its_check_in_owes_none() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    let due = st.next_deadline().unwrap();
    assert!(effects(st.step(Event::CheckInDue, &dead(due, &s, &[]))).is_empty());
    assert_eq!(entry(&st).check_in, CheckIn::ProcessGone);
    assert_eq!(st.next_deadline(), None, "never due again");
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
    assert_eq!((renewed.started, renewed.since), (T0, due + 1));
    assert_eq!(renewed.check_in, CheckIn::Armed);
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
    blocked.asking = true;
    assert_eq!(st.recovery_quiet(&blocked), Some(Quiet::Blocked));
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

/// Past its deadline an entry quiets nothing, its check-in lost or not.
#[test]
fn received_work_is_quiet_until_the_deadline_even_when_the_check_in_is_lost() {
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
    let check_ins = check_ins(effects(st.step(Event::CheckInDue, &at)));
    st.step(
        Event::CheckInUndeliverable {
            check_ins,
            why: "its pane was not found".into(),
        },
        &at,
    );
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

#[test]
fn entries_due_together_share_one_check_in_and_a_stale_result_is_discarded() {
    let s = session();
    let mut st = SessionState::new(s.clone());
    st.step(run("test run", 7001), &facts(T0, &s, &[]));
    st.step(declare(SUBAGENT_NAME), &facts(T0, &s, &[]));
    let due = st.next_deadline().unwrap();
    let fired = check_ins(effects(st.step(Event::CheckInDue, &facts(due, &s, &[]))));
    assert_eq!(fired.len(), 2);
    st.step(declare(SUBAGENT_NAME), &facts(due + 1, &s, &[]));
    let lines = st
        .may_type_check_in(&fired, &facts(due + 1, &s, &[]))
        .unwrap();
    assert_eq!(lines.len(), 1, "the renewed one dropped out");
    assert!(lines[0].run);
    st.step(
        Event::CheckInUndeliverable {
            check_ins: fired,
            why: "no pane".into(),
        },
        &facts(due + 2, &s, &[]),
    );
    assert_eq!(
        st.attendances[0].check_in,
        CheckIn::Undeliverable("no pane".into())
    );
    assert_eq!(
        st.attendances[1].check_in,
        CheckIn::Armed,
        "the renewal keeps its own"
    );
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
    renewed.step(declare(SUBAGENT_NAME), &facts(due + 1, &s, &[]));
    for st in [cleared, renewed] {
        assert_eq!(
            st.may_type_check_in(&ours, &facts(due + 1, &s, &[])),
            Err("the attendance it was for was cleared or renewed".to_string())
        );
    }
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
    let bound = |name: &str, task: &str, since, expect, check_in| Entry {
        work: Attending::Declared {
            name: name.into(),
            task: Some(task.into()),
            misses: 0,
        },
        desc: name.into(),
        started: since,
        since,
        expect,
        check_in,
    };
    let declared = |name: &str, since, expect, check_in| Entry {
        work: Attending::declared(name.into()),
        desc: name.into(),
        started: since,
        since,
        expect,
        check_in,
    };
    let test_run = |since, expect| Entry {
        work: Attending::Run(proof_of(7001)),
        desc: "test run".into(),
        started: since,
        since,
        expect,
        check_in: CheckIn::Armed,
    };
    assert!(restore(old_bytes::NONE).attendances.is_empty());
    assert_eq!(restore(old_bytes::NONE).next_seq, 3);
    assert_eq!(
        restore(old_bytes::INTENT).attendances,
        [test_run(T0, 10 * MINUTE)]
    );
    assert_eq!(
        restore(old_bytes::OTHER_RENEWAL).attendances,
        [
            bound(SUBAGENT_NAME, SUBAGENT_ID, T0, 10 * MINUTE, CheckIn::Armed),
            test_run(T0 + 2 * MINUTE, 20 * MINUTE),
        ]
    );
    let listed = restore(old_bytes::LISTED);
    assert_eq!(
        listed.attendances,
        [
            bound(SUBAGENT_NAME, SUBAGENT_ID, T0, 10 * MINUTE, CheckIn::Armed),
            test_run(T0 + MINUTE, 10 * MINUTE),
            bound(
                "watcher",
                "m1",
                T0,
                10 * MINUTE,
                CheckIn::Withheld(Withheld::AutoOff)
            ),
        ]
    );
    assert_eq!(listed.ended_runs(&DEAD, T0 + 2 * MINUTE).len(), 1);

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
            declared(SUBAGENT_NAME, T0, 10 * MINUTE, CheckIn::Armed),
            declared("reviewer", T0, 10 * MINUTE, CheckIn::Fired),
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
    })
}

/// Grok's native Stop resolves and ends declarations by its own listing,
/// for its own session only, and its waiter verdict is the handoff's
/// alone: the listing never says whether a wait runs. A run outlives it.
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
    assert_eq!(names(&st), ["test run"], "Grok stopped listing it");
    assert_eq!(
        verdict(st.step(
            native(&s, true, TaskSnapshot::Unavailable),
            &facts(T0, &s, &[])
        )),
        Verdict::Silent(SilentWhy::WaitArmed)
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
/// registration, writes no terminal record and owes no check-in as gone,
/// but quiets nothing while unseen.
#[test]
fn an_unseen_run_is_kept_and_never_ended() {
    let s = session();
    let a = owed("event:a", "read:a");
    let mut st = received_then(&s, &a, run("test run", 7001));
    assert!(st.ended_runs(&UNSEEN, T0 + MINUTE).is_empty());
    let unseen = Facts {
        liveness: &UNSEEN,
        ..facts(T0 + MINUTE, &s, &a)
    };
    assert_eq!(st.recovery_quiet(&unseen), None);
    let due = st.next_deadline().unwrap();
    let fired = effects(st.step(
        Event::CheckInDue,
        &Facts {
            liveness: &UNSEEN,
            ..facts(due, &s, &a)
        },
    ));
    assert_eq!(fired.len(), 1, "it may still be running: checked in on");
    assert_eq!(names(&st), ["test run"]);
    assert_eq!(
        st.ended_runs(&DEAD, due + 1).len(),
        1,
        "once seen gone, it ends"
    );
}
