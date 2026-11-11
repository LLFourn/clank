//! What the monitor says of an agent's reachability and activity, through
//! its own seams: the connection table [`waiters`] reads, custody's durable
//! Stop, and recovery's one standing.

use super::*;
use crate::cli::agent_state::{
    Activity, Alarm, Case, HandsOff, Reachability, UNREACHABLE_AFTER, View,
};
use crate::cli::recovery::{Stall, Transport};
use crate::cli::turn_custody::Turns;
use crate::cli::turn_wire::{Report, WireVerdict};
use clank_core::Tool;

const SESSION: &str = "session-one";
const WARN: u64 = 2400;
/// Long enough served that an agent with no connection has no waiter.
const SETTLED: Duration = WAITERS_SETTLE;

const CODEX_STARTED: &str =
    r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}"#;
const CODEX_SAID: &str =
    r#"{"type":"event_msg","payload":{"type":"agent_message","turn_id":"T","message":"FINISHED"}}"#;
const GROK_STARTED: &str = r#"{"type":"turn_started","turn_number":1}"#;
const GROK_ENDED: &str = r#"{"type":"turn_ended","outcome":"completed"}"#;

fn codex(line: &str, turn: &str) -> String {
    line.replace("\"T\"", &format!("\"{turn}\""))
}

fn claude_prompt(id: &str) -> String {
    format!(r#"{{"type":"user","promptId":"{id}","message":{{"content":"go"}}}}"#)
}

fn agent() -> AgentLabel {
    AgentLabel::parse("agent").unwrap()
}

/// One agent as the monitor holds it: its connections, custody of its
/// turns, and recovery reading its record.
struct Reached {
    repo: tempfile::TempDir,
    tool: Tool,
    record: std::path::PathBuf,
    turns: Turns,
    recovery: crate::cli::recovery::Driver,
    clients: Vec<Client>,
    sockets: std::collections::BTreeMap<u64, tokio::sync::mpsc::Receiver<String>>,
}

impl Reached {
    fn new(tool: Tool) -> Self {
        if tool != Tool::Grok {
            crate::cli::recovery::tests::enable(tool, Some(Transport::Typed));
        }
        let repo = tempfile::tempdir().unwrap();
        crate::cli::agent::add_repo_roster_agent(
            repo.path(),
            &agent(),
            crate::cli::teams_config::AgentDescription {
                tool,
                launch: None,
                initial_prompt: None,
            },
            crate::cli::teams_config::RosterRole::Commit,
        )
        .unwrap();
        crate::cli::agent::set_repo_master(repo.path(), &agent()).unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &agent(),
            tool,
            &clank_core::SessionId::parse(SESSION).unwrap(),
        )
        .unwrap();
        crate::agent_store::set_auto_mode(repo.path(), &agent(), clank_core::AutoMode::On).unwrap();
        let record = match tool {
            Tool::Grok => repo.path().join("grok").join("events.jsonl"),
            _ => repo.path().join("record.jsonl"),
        };
        Self::resumed(repo, tool, record)
    }

    fn resumed(repo: tempfile::TempDir, tool: Tool, record: std::path::PathBuf) -> Self {
        Reached {
            turns: Turns::new(repo.path().to_path_buf()),
            recovery: crate::cli::recovery::Driver::new("/".into(), "/nonexistent-home".into()),
            repo,
            tool,
            record,
            clients: Vec::new(),
            sockets: Default::default(),
        }
    }

    /// The same agent, record and disk under a monitor started afresh,
    /// which holds no connection yet.
    fn restart(self) -> Self {
        Self::resumed(self.repo, self.tool, self.record)
    }

    /// The record gains `lines`, written at `at`.
    fn write(&mut self, lines: &[String], at: u64) {
        use std::io::Write as _;
        std::fs::create_dir_all(self.record.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.record)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(at))
            .unwrap();
        self.recovery.changed(
            crate::cli::recovery::RecordEvent::Changed {
                watch: crate::cli::recovery::WatchKey {
                    dir: self.record.parent().unwrap().to_path_buf(),
                    recursive: false,
                },
                path: self.record.clone(),
            },
            at,
        );
    }

    fn holder(&self, session: &str) -> Option<Holder> {
        Some(Holder {
            tool: self.tool,
            session: session.into(),
        })
    }

    fn connect(
        &mut self,
        who: Served,
        holder: Option<Holder>,
        report: Option<Report>,
        at: u64,
    ) -> u64 {
        let (out, socket) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let conn = next_conn();
        self.clients.push(Client {
            who,
            holder,
            out,
            writer: None,
            farewell: None,
            shown: Default::default(),
            conn,
            admitted: Admitted {
                at,
                build: Some("build-of-the-waiter".into()),
            },
            report: report.map(|report| {
                Box::new(Reporting {
                    report,
                    stepped: true,
                    reader: None,
                })
            }),
        });
        self.sockets.insert(conn, socket);
        conn
    }

    /// The Stop the agent's hook would report now: placed where the hook
    /// reads its record's newest turn boundary.
    fn stop_report(&self, turn: &str) -> Report {
        Report::TurnEnd {
            stop: clank_core::turn_policy::TurnEndReport {
                tool: self.tool,
                session: clank_core::SessionId::parse(SESSION).unwrap(),
                place: crate::cli::wake_repeat::place(self.tool, Some(&self.record), Some(turn)),
                tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
                hook_started: None,
            },
            deadline: u64::MAX,
            questions: Vec::new(),
        }
    }

    /// The agent's Stop hook reports `turn`'s end with nothing owed, is
    /// parked, and stays connected.
    fn stop(&mut self, turn: &str, at: u64) -> u64 {
        let report = self.stop_report(turn);
        self.stop_with(report, at)
    }

    fn stop_with(&mut self, report: Report, at: u64) -> u64 {
        let g = self.gathered(false);
        let conn = self.connect(
            Served::Agent { author: agent() },
            self.holder(SESSION),
            Some(report.clone()),
            at,
        );
        let frame = self
            .turns
            .answer(&agent(), conn, &report, true, &g)
            .expect("answered");
        assert_eq!(frame.verdict, WireVerdict::Park, "nothing owed: parked");
        conn
    }

    /// The connection's peer goes, as a hook killed with its harness does.
    fn hang_up(&mut self, conn: u64) {
        self.sockets.remove(&conn);
    }

    fn gathered(&self, owed: bool) -> crate::cli::turn_custody::Gathered {
        let items = if owed {
            vec![clank_core::wait::WaitItem::Message {
                presentation: None,
                requires_reply: false,
                reply_route: None,
                handle: "m1".into(),
                from: None,
                repo: None,
                subject: "Rebase please".into(),
                body: "onto master".into(),
                selector: Some("messages@m1".into()),
            }]
        } else {
            vec![]
        };
        crate::cli::turn_custody::Gathered::of(self.repo.path(), &agent(), Some((&items, None)))
    }

    fn session(&self) -> clank_core::agent_config::Session {
        serde_json::from_value(serde_json::json!({
            "id": SESSION,
            "tool": self.tool.as_str(),
            "updated_at": "2026-11-09T00:00:00Z",
            "transcript": match self.tool {
                Tool::Grok => self.record.with_file_name("chat.jsonl"),
                _ => self.record.clone(),
            },
        }))
        .unwrap()
    }

    /// One round of the loop at `now` by a monitor that has `served` this
    /// long: its connections tended, recovery told the table, and the fold
    /// reconciled with a message owed since `owed` or nothing.
    fn round(&mut self, owed: Option<u64>, now: u64, served: Duration) {
        use crate::cli::ledger::{Delivery, Entry, Ledger, Obligation, Since};
        let _ = tend(
            self.repo.path(),
            &mut self.clients,
            None,
            &[],
            &Default::default(),
            &mut self.turns,
            &mut self.recovery,
        );
        self.recovery.reach(waiters(&self.clients, served), now);
        let g = self.gathered(owed.is_some());
        let facts = crate::cli::recovery::Facts {
            author: agent(),
            agent_dir: self.dir(),
            session: self.session(),
            auto: true,
            ledger: Ledger {
                entries: owed
                    .map(|since| Entry {
                        identity: "m1".into(),
                        kind: "message",
                        what: "message from claude".into(),
                        obligation: Obligation::Owed {
                            owed_since: Since::At(since),
                        },
                        delivery: Delivery::NotDelivered,
                    })
                    .into_iter()
                    .collect(),
                last_delivery: None,
            },
            role: "master".into(),
            work: owed.map(|_| "m1".to_string()).into_iter().collect(),
            blind_nudge: None,
            stall: Some(Stall {
                warn: WARN,
                nudge: Some(300),
            }),
            attended: false,
            blocked_on: None,
            asking: false,
            projection: self.turns.projection(&agent(), &g),
        };
        self.recovery.reconcile(vec![facts], now);
    }

    fn dir(&self) -> std::path::PathBuf {
        crate::agent_store::agents_root(self.repo.path()).join("agent")
    }

    fn view(&self, now: u64) -> View {
        View::at(
            crate::cli::agent_state::load(&self.dir()).unwrap(),
            None,
            now,
        )
    }

    fn dispatched_at(&mut self, now: u64) -> Vec<crate::cli::recovery::AttemptRef> {
        self.recovery
            .due(now, |_, _| crate::cli::recovery::Gate::Go)
            .into_iter()
            .filter_map(|a| match a {
                crate::cli::recovery::Act::Dispatch { attempt, .. } => Some(attempt),
                _ => None,
            })
            .collect()
    }

    /// The last look's own view of the agent at a typed send.
    fn may_send(&mut self, attempt: &crate::cli::recovery::AttemptRef) -> bool {
        let live = crate::cli::recovery::Live {
            role: "master".into(),
            work: vec!["m1".into()],
            bound: Some(crate::cli::recovery::Bound::of(&self.session())),
            transport: Transport::Typed,
            typing_into: Some(self.tool),
        };
        self.recovery.may_send(&agent(), attempt, &live)
    }

    /// A Codex agent whose turn `t1` its record leaves open, and whose
    /// Stop hook reported that turn's end at 1000 and is parked.
    fn stopped_codex() -> (Self, u64) {
        let mut a = Reached::new(Tool::Codex);
        a.write(&[codex(CODEX_STARTED, "t1"), codex(CODEX_SAID, "t1")], 1000);
        let conn = a.stop("t1", 1000);
        (a, conn)
    }
}

/// The artist4 case. A Codex record leaves a turn open whose Stop hook
/// reported its end, durably, and the monitor that follows holds no waiter
/// for the agent. It is unreachable, never working; red once a message is
/// owed; and nudged after an idle agent's settle, not a running turn's
/// stall window.
#[tokio::test]
async fn a_stopped_turn_with_no_waiter_is_unreachable_and_recovered_as_idle() {
    let (a, _parked) = Reached::stopped_codex();
    let stored = std::fs::read_to_string(a.dir().join("turn.json")).unwrap();
    assert!(
        stored.contains(r#""stopped":{"turn":"t1""#),
        "the Stop is in the turn state: {stored}"
    );

    let mut b = a.restart();
    b.round(None, 50_000, SETTLED);
    let seen = b.view(50_000);
    assert_eq!(seen.state.activity, Activity::Idle, "its Stop said so");
    assert_eq!(
        seen.state.reachability,
        Reachability::NoWaiter { since: None },
        "no record dates a waiter this monitor never held"
    );
    assert!(!seen.working(50_000));
    assert_eq!(seen.line(50_000), "unreachable age unknown");
    assert_eq!(seen.alarm(50_000), None, "nothing owed: dim");

    b.round(Some(50_100), 50_100, SETTLED);
    let owed = b.view(50_100);
    assert_eq!(owed.alarm(50_100), Some(Alarm::Unreachable), "owed: red");
    assert_eq!(
        owed.state.owed.as_ref().map(|o| &o.case),
        Some(&Case::Recovering)
    );
    assert!(
        owed.row(50_100)
            .is_some_and(|row| row.starts_with("unreachable age unknown · nudge in")),
        "{:?}",
        owed.row(50_100)
    );
    assert!(b.dispatched_at(50_100 + 179).is_empty(), "it settles first");
    let nudged = b.dispatched_at(50_100 + 180);
    assert_eq!(nudged.len(), 1, "long before the stall window of {WARN}s");
    assert!(b.may_send(&nudged[0]));
}

/// A turn whose Stop was reported, on a waiter that is still connected,
/// is the resting state: the row says nothing. When that waiter goes the
/// agent is unreachable from then, once that has lasted.
#[tokio::test]
async fn a_stopped_turn_on_a_connected_waiter_rests_until_the_waiter_goes() {
    let (mut a, parked) = Reached::stopped_codex();
    a.round(None, 1010, Duration::ZERO);
    let resting = a.view(1010);
    assert_eq!(
        resting.state.reachability,
        Reachability::Waiter {
            since: 1000,
            build: Some("build-of-the-waiter".into()),
        },
        "the waiter's own connection says when, and which build"
    );
    assert_eq!(resting.row(1010), None);
    assert_eq!(resting.line(1010), "waiting for work");

    a.hang_up(parked);
    a.round(None, 5000, Duration::ZERO);
    let gone = a.view(5000);
    assert_eq!(
        gone.state.reachability,
        Reachability::NoWaiter { since: Some(5000) },
        "seen going, whether or not the monitor has settled"
    );
    assert_eq!(gone.state.activity, Activity::Idle);
    // The close is published on the loop's own clock, which is where the
    // turn was first seen over.
    let from = gone.state.since.max(5000);
    assert_eq!(gone.row(from + UNREACHABLE_AFTER - 1), None);
    assert_eq!(
        gone.row(from + UNREACHABLE_AFTER).as_deref(),
        Some("unreachable 20s")
    );
}

/// Only an admitted deliverer of the session and tool the label is bound
/// to now is a waiter. An observer, a recovery helper, a wait holding no
/// session, a report answered once, a connection of a binding since
/// replaced or of another tool, and one whose socket closed are none.
#[tokio::test]
async fn only_a_deliverer_of_the_current_binding_is_a_waiter() {
    let (mut a, parked) = Reached::stopped_codex();
    a.hang_up(parked);
    a.round(None, 1010, SETTLED);
    let session_start = Report::SessionStart {
        tool: Tool::Codex,
        session: clank_core::SessionId::parse(SESSION).unwrap(),
        deadline: u64::MAX,
    };
    let mine = || Served::Agent { author: agent() };
    a.connect(Served::Observer, None, None, 1020);
    a.connect(
        Served::Helper { author: agent() },
        a.holder(SESSION),
        None,
        1020,
    );
    a.connect(mine(), None, None, 1020);
    a.connect(mine(), a.holder(SESSION), Some(session_start), 1020);
    a.connect(mine(), a.holder("session-zero"), None, 1020);
    a.connect(
        mine(),
        Some(Holder {
            tool: Tool::Claude,
            session: SESSION.into(),
        }),
        None,
        1020,
    );
    let closed = a.connect(mine(), a.holder(SESSION), None, 1020);
    a.hang_up(closed);
    let held: Vec<_> = waiters(&a.clients, SETTLED)
        .connected
        .into_values()
        .flatten()
        .map(|w| (w.tool, w.session))
        .collect();
    assert_eq!(
        held,
        [
            (Tool::Codex, "session-zero".to_string()),
            (Tool::Claude, SESSION.to_string())
        ],
        "only deliverers are in the table, each with the binding it holds"
    );
    a.round(None, 1030, SETTLED);
    assert!(
        matches!(
            a.view(1030).state.reachability,
            Reachability::NoWaiter { .. }
        ),
        "and none of them holds the agent's binding: {:?}",
        a.view(1030).state.reachability
    );

    a.connect(mine(), a.holder(SESSION), None, 1040);
    a.round(None, 1040, SETTLED);
    assert_eq!(
        a.view(1040).state.reachability,
        Reachability::Waiter {
            since: 1040,
            build: Some("build-of-the-waiter".into()),
        },
        "a wait holding the bound session is one"
    );
}

/// A monitor that has not served long enough for a surviving waiter to
/// have found it does not know that an agent has none.
#[tokio::test]
async fn a_missing_connection_is_no_waiter_only_once_the_monitor_has_settled() {
    let (a, _parked) = Reached::stopped_codex();
    let mut b = a.restart();
    b.round(None, 50_000, WAITERS_SETTLE - Duration::from_secs(1));
    let early = b.view(50_000);
    assert_eq!(early.state.reachability, Reachability::Unknown);
    assert_eq!(early.row(50_000).as_deref(), Some("unknown"));
    b.round(None, 50_001, WAITERS_SETTLE);
    assert_eq!(
        b.view(50_001).state.reachability,
        Reachability::NoWaiter { since: None }
    );
}

/// Reachability is the monitor's connection table and nothing else: a
/// state file left claiming a waiter is not one.
#[tokio::test]
async fn a_published_file_claiming_a_waiter_is_not_a_connection() {
    let (mut a, _parked) = Reached::stopped_codex();
    a.round(None, 1010, SETTLED);
    assert!(matches!(
        a.view(1010).state.reachability,
        Reachability::Waiter { .. }
    ));
    let mut b = a.restart();
    let claimed = std::fs::read_to_string(b.dir().join("state.json")).unwrap();
    assert!(claimed.contains(r#""by": "waiter""#), "{claimed}");
    b.round(None, 50_000, Duration::ZERO);
    assert_eq!(
        b.view(50_000).state.reachability,
        Reachability::Unknown,
        "before it has settled the file says nothing either"
    );
    b.round(None, 50_040, SETTLED);
    assert_eq!(
        b.view(50_040).state.reachability,
        Reachability::NoWaiter { since: None }
    );
}

/// A Grok agent whose turn is over with no wait armed, once a monitor is
/// serving again, is unreachable and says clank cannot resume it. Nothing
/// is typed at it, however long it is owed work.
#[tokio::test]
async fn an_unreachable_grok_agent_is_shown_and_never_typed_at() {
    let mut g = Reached::new(Tool::Grok);
    g.write(&[GROK_STARTED.into(), GROK_ENDED.into()], 1000);
    g.round(Some(1000), 1010, SETTLED);
    let seen = g.view(1010 + UNREACHABLE_AFTER);
    assert_eq!(seen.state.activity, Activity::Idle);
    assert_eq!(
        seen.state.owed.as_ref().map(|o| &o.case),
        Some(&Case::HandsOff(HandsOff::CannotResume(
            crate::cli::resume::NO_CANDIDATE.into()
        )))
    );
    assert_eq!(
        seen.line(1010 + UNREACHABLE_AFTER),
        "unreachable age unknown · not nudged: this harness has no way to resume that cancels \
         nothing"
    );
    assert_eq!(
        seen.alarm(1010 + UNREACHABLE_AFTER),
        Some(Alarm::Unreachable),
        "owed work: red"
    );
    for now in [1010 + 180, 1010 + WARN, 1010 + 100 * WARN] {
        g.round(Some(1000), now, SETTLED);
        assert!(g.dispatched_at(now).is_empty(), "{now}");
    }
    assert_eq!(
        crate::cli::recovery::load(&g.dir()),
        None,
        "no episode is opened to nudge it"
    );
    assert!(crate::cli::nudge_log::read(&g.dir()).is_empty());
}

/// The race that forbids typing. An agent with a durable Stop and no
/// waiter is claimed a nudge; before it is sent the agent goes on, by a
/// new turn or by more of the same one. The Stop no longer speaks for the
/// record's latest turn, and the last look refuses the send.
#[tokio::test]
async fn a_turn_begun_between_the_look_and_the_send_is_never_typed_into() {
    type GoesOn = fn(&mut Reached, u64);
    let goes_on: [(Tool, &str, GoesOn); 3] = [
        (Tool::Codex, "a new turn", |a, at| {
            a.write(&[codex(CODEX_STARTED, "t2")], at)
        }),
        (Tool::Codex, "more of the same turn", |a, at| {
            a.write(&[codex(CODEX_SAID, "t1")], at)
        }),
        (Tool::Claude, "a new turn", |a, at| {
            a.write(&[claude_prompt("t2")], at)
        }),
    ];
    for (tool, how, go_on) in goes_on {
        let claimed = |change: Option<GoesOn>| {
            let mut a = Reached::new(tool);
            match tool {
                Tool::Claude => a.write(&[claude_prompt("t1")], 1000),
                _ => a.write(&[codex(CODEX_STARTED, "t1"), codex(CODEX_SAID, "t1")], 1000),
            }
            let parked = a.stop("t1", 1000);
            a.hang_up(parked);
            a.round(Some(1100), 1100, SETTLED);
            assert_eq!(a.view(1100).state.activity, Activity::Idle, "{tool:?}");
            let attempts = a.dispatched_at(1100 + 180);
            let [attempt] = attempts.as_slice() else {
                panic!("{tool:?}: one nudge is claimed: {attempts:?}");
            };
            if let Some(change) = change {
                change(&mut a, 1300);
            }
            let sent = a.may_send(attempt);
            (a, sent)
        };
        let (_, sent) = claimed(None);
        assert!(sent, "{tool:?}: with the agent where it was, it is sent");
        let (a, sent) = claimed(Some(go_on));
        assert!(!sent, "{tool:?}: {how}");
        assert!(
            matches!(a.view(1300).state.activity, Activity::Working(_)),
            "{tool:?}, {how}: the turn the record leaves open is its own again: {:?}",
            a.view(1300).state.activity
        );
    }
}

/// A turn that really is going: open in the record, no Stop reported for
/// it, and the record moving inside the stall window. Past the window
/// with nothing written it is a turn that is open, and no longer work.
#[tokio::test]
async fn an_open_turn_is_working_only_while_its_record_moves() {
    let mut a = Reached::new(Tool::Codex);
    a.write(&[codex(CODEX_STARTED, "t1")], 1000);
    a.round(None, 1010, SETTLED);
    let seen = a.view(1010);
    assert_eq!(
        seen.state.activity,
        Activity::Working(crate::cli::agent_state::Moving {
            moved: 1000,
            until: 1000 + WARN,
        })
    );
    assert!(seen.working(1010));
    assert_eq!(seen.line(1010), "working 0s");
    assert!(
        !seen.unreachable(1010 + WARN),
        "a working agent has no waiter"
    );

    let before = std::fs::read(a.dir().join("state.json")).unwrap();
    a.write(&[codex(CODEX_SAID, "t1")], 1100);
    assert_eq!(
        std::fs::read(a.dir().join("state.json")).unwrap(),
        before,
        "evidence still good for most of its window is not rewritten"
    );
    assert_eq!(a.recovery.evidence_due(), Some(1000 + WARN / 2));
    a.dispatched_at(1000 + WARN / 2);
    assert_eq!(
        a.view(0).state.activity,
        Activity::Working(crate::cli::agent_state::Moving {
            moved: 1100,
            until: 1100 + WARN,
        }),
        "renewed from the record at its half life"
    );
    assert_eq!(a.recovery.evidence_due(), Some(1100 + WARN / 2));
    a.dispatched_at(1100 + WARN / 2);
    assert_eq!(a.recovery.evidence_due(), Some(1100 + WARN));
    a.dispatched_at(1100 + WARN);
    let silent = a.view(1100 + WARN);
    assert_eq!(silent.state.activity, Activity::TurnOpen);
    assert!(!silent.working(1100 + WARN));
    assert_eq!(silent.line(1100 + WARN), "turn open 41m");
    assert_eq!(a.recovery.evidence_due(), None);
}

/// A hook woken at its deadline is past the Stop it reported.
#[tokio::test]
async fn a_hook_woken_at_its_deadline_is_past_its_stop() {
    let mut a = Reached::new(Tool::Codex);
    a.write(&[codex(CODEX_STARTED, "t1"), codex(CODEX_SAID, "t1")], 1000);
    let Report::TurnEnd { stop, .. } = a.stop_report("t1") else {
        unreachable!()
    };
    let report = Report::TurnEnd {
        stop,
        deadline: 2_000_000,
        questions: Vec::new(),
    };
    let g = a.gathered(false);
    let conn = a.connect(
        Served::Agent { author: agent() },
        a.holder(SESSION),
        Some(report.clone()),
        1000,
    );
    a.turns.answer(&agent(), conn, &report, true, &g).unwrap();
    assert!(a.turns.state(&agent()).unwrap().stopped.is_some());
    let _ = expire_reports(&mut a.clients, Some(&mut a.turns), 1_999_999);
    assert!(
        a.turns.state(&agent()).unwrap().stopped.is_some(),
        "not yet"
    );
    let _ = expire_reports(&mut a.clients, Some(&mut a.turns), 2_000_000);
    assert!(a.clients.is_empty(), "told to re-arm, and retired");
    assert_eq!(a.turns.state(&agent()).unwrap().stopped, None);
    let stored = std::fs::read_to_string(a.dir().join("turn.json")).unwrap();
    assert!(!stored.contains("stopped"), "{stored}");
}

/// A monitor that never held the agent's waiter cannot date its going,
/// but a turn it sees end with none dates the wait from there.
#[tokio::test]
async fn a_turn_seen_ending_with_no_waiter_dates_the_wait() {
    const CODEX_DONE: &str =
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"T","error":null}}"#;
    let mut a = Reached::new(Tool::Codex);
    a.write(&[codex(CODEX_STARTED, "t1")], 1000);
    a.round(None, 1010, SETTLED);
    assert_eq!(
        a.view(1010).state.reachability,
        Reachability::NoWaiter { since: None }
    );
    a.write(&[codex(CODEX_DONE, "t1")], 1500);
    let over = a.view(1500);
    assert_eq!(over.state.activity, Activity::Idle);
    assert_eq!(
        over.state.reachability,
        Reachability::NoWaiter { since: Some(1500) }
    );
    assert_eq!(over.row(1500 + UNREACHABLE_AFTER - 1), None);
    assert_eq!(over.line(1500 + 300), "unreachable 5m");
}

/// A Claude tool result of `bytes` for turn `id`: a record of the turn
/// that is not its prompt.
fn claude_result(id: &str, bytes: usize) -> String {
    format!(
        r#"{{"type":"user","promptId":"{id}","message":{{"content":[{{"type":"tool_result","content":"{}"}}]}}}}"#,
        "x".repeat(bytes)
    )
}

const BEYOND_THE_SCAN: usize = 300 * 1024;

/// A moment after any Stop these tests report: the monitor dates a Stop
/// on its own clock.
fn after_the_stop() -> u64 {
    crate::cli::recovery::unix_now() + 60
}

/// A Claude turn `p1` so long that its prompt is further back than the
/// hook's scan, with its Stop reported and parked: the hook can name the
/// turn by its later records and nothing more.
fn long_claude_turn() -> (Reached, u64) {
    let mut a = Reached::new(Tool::Claude);
    a.write(&[claude_prompt("p1")], 1000);
    a.write(
        &[claude_result("p1", BEYOND_THE_SCAN), claude_result("p1", 8)],
        1050,
    );
    assert_eq!(
        crate::cli::wake_repeat::claude_boundary(&a.record),
        None,
        "the prompt is outside the adapter's scan"
    );
    let report = a.stop_report("p1");
    let Report::TurnEnd { stop, .. } = &report else {
        unreachable!()
    };
    assert_eq!(
        (stop.place.turn.as_deref(), stop.place.position),
        (Some("p1"), None),
        "the turn is named, its boundary is not"
    );
    let conn = a.stop_with(report, 1110);
    (a, conn)
}

/// A Stop the adapter could not place is still a Stop: the turn its
/// record leaves open is not working. With nothing written since the
/// report it is over, resting on its waiter and recovered as idle; under
/// the monitor that follows, with the waiter gone, it is unreachable.
#[tokio::test]
async fn an_unplaced_stop_is_never_working_under_this_monitor_or_the_next() {
    let (mut a, _parked) = long_claude_turn();
    a.round(Some(1000), 1110, SETTLED);
    let seen = a.view(1110);
    assert_eq!(seen.state.activity, Activity::Idle);
    assert!(!seen.working(1110));
    assert!(matches!(
        seen.state.reachability,
        Reachability::Waiter { .. }
    ));
    assert_eq!(
        seen.state.owed.as_ref().map(|o| &o.case),
        Some(&Case::Recovering),
        "an idle agent's standing, not an active turn's"
    );
    let stored = std::fs::read_to_string(a.dir().join("turn.json")).unwrap();
    assert!(
        stored.contains(r#""stopped":{"turn":"p1","position":null"#),
        "{stored}"
    );

    let mut b = a.restart();
    b.round(Some(1000), 50_000, SETTLED);
    let next = b.view(50_000);
    assert_eq!(next.state.activity, Activity::Idle, "after a restart");
    assert!(!next.working(50_000));
    assert_eq!(
        next.state.reachability,
        Reachability::NoWaiter { since: None }
    );
    assert_eq!(next.alarm(50_000), Some(Alarm::Undelivered));
    assert_eq!(b.dispatched_at(50_000 + 180).len(), 1);
}

/// A Stop from an adapter that names no turn, no boundary and nothing it
/// saw of the record ends no turn: the open turn is unknown, never idle.
/// Once the record is written after the monitor was told, the Stop says
/// nothing of what is running.
#[tokio::test]
async fn a_stop_that_names_no_turn_speaks_only_while_nothing_is_written() {
    let mut a = Reached::new(Tool::Claude);
    a.write(&[claude_prompt("p1")], 1000);
    let Report::TurnEnd { mut stop, .. } = a.stop_report("p1") else {
        unreachable!()
    };
    stop.place = Default::default();
    let parked = a.stop_with(
        Report::TurnEnd {
            stop,
            deadline: u64::MAX,
            questions: Vec::new(),
        },
        1110,
    );
    a.round(Some(1000), 1110, SETTLED);
    let seen = a.view(1110);
    assert!(
        matches!(seen.state.activity, Activity::Unknown(_)),
        "{:?}",
        seen.state.activity
    );
    assert!(a.dispatched_at(1110 + 180).is_empty());

    a.hang_up(parked);
    let later = after_the_stop();
    a.write(&[claude_result("p1", 8)], later);
    assert!(
        matches!(a.view(later).state.activity, Activity::Working(_)),
        "{:?}",
        a.view(later).state.activity
    );
}

/// A turn that begins after an unplaced Stop is its own: running, and
/// working while its record moves. The Stop named another turn.
#[tokio::test]
async fn a_turn_begun_after_an_unplaced_stop_is_running_again() {
    let (mut a, parked) = long_claude_turn();
    a.round(Some(1000), 1110, SETTLED);
    a.hang_up(parked);
    a.round(Some(1000), 1120, SETTLED);
    let later = after_the_stop();
    a.write(&[claude_prompt("p2")], later);
    a.round(Some(1000), later, SETTLED);
    let seen = a.view(later);
    assert!(
        matches!(seen.state.activity, Activity::Working(_)),
        "{:?}",
        seen.state.activity
    );
    assert_eq!(
        seen.state.owed.as_ref().map(|o| &o.case),
        Some(&Case::Active(crate::cli::agent_state::Active::Turn))
    );
    let mut b = a.restart();
    b.round(Some(1000), later, SETTLED);
    assert!(
        matches!(b.view(later).state.activity, Activity::Working(_)),
        "after a restart the old Stop still says nothing of the new turn"
    );
}

/// The turn an unplaced Stop named, written to again since the report,
/// cannot be placed: the Stop may be behind those writes or ahead of them.
/// It is unknown, to the row and to recovery alike: not working, not idle,
/// and nothing is typed at it.
#[tokio::test]
async fn the_stopped_turn_written_since_an_unplaced_stop_is_unknown() {
    let (mut a, parked) = long_claude_turn();
    a.round(Some(1000), 1110, SETTLED);
    a.hang_up(parked);
    a.round(Some(1000), 1120, SETTLED);
    let later = after_the_stop();
    a.write(&[claude_result("p1", 8)], later);
    let unknown = |m: &mut Reached| {
        m.round(Some(1000), later, SETTLED);
        let seen = m.view(later);
        assert!(
            matches!(seen.state.activity, Activity::Unknown(_)),
            "{:?}",
            seen.state.activity
        );
        assert!(!seen.working(later));
        assert_eq!(
            seen.state.owed.as_ref().map(|o| &o.case),
            Some(&Case::HandsOff(HandsOff::BlindNudgeOff)),
            "recovery reads the same unknown"
        );
        for now in [later + 180, later + WARN, later + 10 * WARN] {
            assert!(m.dispatched_at(now).is_empty(), "{now}");
        }
    };
    unknown(&mut a);
    unknown(&mut a.restart());
}

/// The race, for a Stop with no place: a nudge claimed for the idle agent
/// is refused at the send once the agent has gone on, by a new turn or by
/// more of the one that stopped.
#[tokio::test]
async fn nothing_is_typed_after_an_unplaced_stop_once_the_agent_goes_on() {
    for (how, line) in [
        (None, None),
        (Some("a new turn"), Some(claude_prompt("p2"))),
        (Some("more of the same turn"), Some(claude_result("p1", 8))),
    ] {
        let (mut a, parked) = long_claude_turn();
        a.hang_up(parked);
        a.round(Some(1100), 1110, SETTLED);
        let attempts = a.dispatched_at(1110 + 180);
        let [attempt] = attempts.as_slice() else {
            panic!("one nudge is claimed: {attempts:?}");
        };
        if let Some(line) = line {
            a.write(&[line], after_the_stop());
        }
        assert_eq!(a.may_send(attempt), how.is_none(), "{how:?}");
    }
}

/// Codex's boundary comes back unplaced the same way: a turn whose start
/// is further back than the scan. Its hook is told the turn, so the Stop
/// names it, and a later turn is running again.
#[tokio::test]
async fn an_unplaced_codex_stop_is_not_working_and_a_later_turn_is() {
    let filler = format!(
        r#"{{"type":"response_item","payload":{{"type":"message","content":"{}"}}}}"#,
        "x".repeat(BEYOND_THE_SCAN)
    );
    let mut a = Reached::new(Tool::Codex);
    a.write(&[codex(CODEX_STARTED, "t1")], 1000);
    a.write(
        &[filler, r#"{"type":"response_item","payload":{}}"#.into()],
        1050,
    );
    assert_eq!(
        crate::cli::wake_repeat::codex_boundary(&a.record, "t1"),
        None
    );
    let parked = a.stop("t1", 1110);
    assert_eq!(
        a.turns
            .state(&agent())
            .unwrap()
            .stopped
            .as_ref()
            .map(|s| (s.turn.as_deref(), s.position)),
        Some((Some("t1"), None))
    );
    a.round(Some(1000), 1110, SETTLED);
    let seen = a.view(1110);
    assert_eq!(seen.state.activity, Activity::Idle);
    assert!(!seen.working(1110));

    a.hang_up(parked);
    a.round(Some(1000), 1120, SETTLED);
    let later = after_the_stop();
    a.write(&[codex(CODEX_STARTED, "t2")], later);
    assert!(
        matches!(a.view(later).state.activity, Activity::Working(_)),
        "{:?}",
        a.view(later).state.activity
    );
}

/// What the agent did between its hook reading the record and the
/// monitor admitting the hook's report.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Meanwhile {
    Nothing,
    NewTurn,
    /// The turn that stopped goes on under its own id.
    SameTurn,
}

/// The hook reads `tool`'s long turn and captures its Stop; the agent
/// then does `meanwhile`; only then is the report admitted, at 1110, with
/// work owed. `named` is whether the report names the turn.
fn captured_then(tool: Tool, named: bool, meanwhile: Meanwhile) -> (Reached, Report) {
    let filler = format!(
        r#"{{"type":"response_item","payload":{{"type":"message","content":"{}"}}}}"#,
        "x".repeat(BEYOND_THE_SCAN)
    );
    let mut a = Reached::new(tool);
    match tool {
        Tool::Claude => {
            a.write(&[claude_prompt("t1")], 1000);
            a.write(
                &[claude_result("t1", BEYOND_THE_SCAN), claude_result("t1", 8)],
                1050,
            );
        }
        _ => {
            a.write(&[codex(CODEX_STARTED, "t1")], 1000);
            a.write(
                &[filler, r#"{"type":"response_item","payload":{}}"#.into()],
                1050,
            );
        }
    }
    let Report::TurnEnd { mut stop, .. } = a.stop_report("t1") else {
        unreachable!()
    };
    assert_eq!(stop.place.position, None, "{tool:?}: unplaced");
    assert!(stop.place.read_to.is_some(), "{tool:?}: but seen");
    if !named {
        stop.place.turn = None;
    }
    let report = Report::TurnEnd {
        stop,
        deadline: u64::MAX,
        questions: Vec::new(),
    };
    let went_on = match (tool, meanwhile) {
        (_, Meanwhile::Nothing) => vec![],
        (Tool::Claude, Meanwhile::NewTurn) => vec![claude_prompt("t2")],
        (Tool::Claude, Meanwhile::SameTurn) => vec![claude_result("t1", 8)],
        (_, Meanwhile::NewTurn) => vec![codex(CODEX_STARTED, "t2")],
        (_, Meanwhile::SameTurn) => vec![codex(CODEX_STARTED, "t1"), codex(CODEX_SAID, "t1")],
    };
    if !went_on.is_empty() {
        a.write(&went_on, 1100);
    }
    (a, report)
}

/// Whether `a` reads as idle at `now` and, if a nudge is claimed for it,
/// whether the last look would send it.
fn idle_and_sent(a: &mut Reached, now: u64) -> (bool, bool) {
    a.round(Some(1000), now, SETTLED);
    let idle = a.view(now).state.activity == Activity::Idle;
    let sent = match a.dispatched_at(now + 180).as_slice() {
        [] => false,
        [attempt] => a.may_send(attempt),
        many => panic!("one nudge at most: {many:?}"),
    };
    (idle, sent)
}

/// The ordering that receipt time cannot see: the hook captures its Stop,
/// the agent goes on, and only then is the report admitted. An unplaced
/// Stop ends a turn only while the record is exactly what its hook saw,
/// so neither a new turn nor the stopped turn resumed under its own id is
/// read as idle or typed into, whether or not the report names the turn;
/// under the monitor that admitted it, the one that follows, or when the
/// same captured report is admitted again. With the record untouched the
/// same report does end the turn, replayed or not, and a park lost with
/// its Stop still standing is recovered as idle.
#[tokio::test]
async fn a_stop_captured_before_the_agent_went_on_never_ends_what_followed() {
    for tool in [Tool::Claude, Tool::Codex] {
        for named in [true, false] {
            for meanwhile in [Meanwhile::Nothing, Meanwhile::NewTurn, Meanwhile::SameTurn] {
                let case = format!("{tool:?}, named {named}, {meanwhile:?}");
                let over = meanwhile == Meanwhile::Nothing;

                // The connection that carried the report is lost: with
                // recovery not yet reading the record, with it already
                // reading before the report is admitted, and after a round
                // has seen the waiter. A loss dates the agent's having no
                // waiter and ends no turn, whenever it is seen.
                for seen_parked in [false, true] {
                    let lost = format!("{case}, reader attached, parked seen {seen_parked}");
                    let reading_then_lost = || {
                        let (mut a, report) = captured_then(tool, named, meanwhile);
                        a.round(Some(1000), 1105, SETTLED);
                        let parked = a.stop_with(report, 1110);
                        if seen_parked {
                            a.round(Some(1000), 1110, SETTLED);
                        }
                        a.hang_up(parked);
                        a
                    };
                    let mut a = reading_then_lost();
                    assert_eq!(idle_and_sent(&mut a, 1110), (over, over), "{lost}");
                    assert!(
                        a.turns.state(&agent()).unwrap().unwatched.is_some(),
                        "{lost}: the loss is dated"
                    );
                    let mut a = reading_then_lost();
                    a.round(Some(1000), 1110, SETTLED);
                    let mut next = a.restart();
                    assert_eq!(
                        idle_and_sent(&mut next, 50_000),
                        (over, over),
                        "{lost}: after a restart"
                    );
                }

                let (mut a, report) = captured_then(tool, named, meanwhile);
                let parked = a.stop_with(report.clone(), 1110);
                a.hang_up(parked);
                assert_eq!(idle_and_sent(&mut a, 1110), (over, over), "{case}");
                if meanwhile == Meanwhile::SameTurn {
                    assert!(
                        matches!(a.view(1110).state.activity, Activity::Unknown(_)),
                        "{case}: neither running nor over: {:?}",
                        a.view(1110).state.activity
                    );
                }
                if (named, meanwhile) == (true, Meanwhile::NewTurn) {
                    assert!(
                        matches!(a.view(1110).state.activity, Activity::Working(_)),
                        "{case}: the newer turn is running: {:?}",
                        a.view(1110).state.activity
                    );
                }

                let (mut a, report) = captured_then(tool, named, meanwhile);
                let parked = a.stop_with(report.clone(), 1110);
                a.hang_up(parked);
                a.round(Some(1000), 1110, SETTLED);
                let mut next = a.restart();
                assert_eq!(
                    idle_and_sent(&mut next, 50_000),
                    (over, over),
                    "{case}: after a restart"
                );

                let (mut a, report) = captured_then(tool, named, meanwhile);
                let first = a.stop_with(report.clone(), 1110);
                a.hang_up(first);
                a.round(Some(1000), 1110, SETTLED);
                let told = a.turns.state(&agent()).unwrap().stopped.clone();
                let mut next = a.restart();
                let again = next.stop_with(report.clone(), 1200);
                assert_eq!(
                    next.turns.state(&agent()).unwrap().stopped,
                    told,
                    "{case}: the same report replayed is the same Stop"
                );
                next.hang_up(again);
                assert_eq!(
                    idle_and_sent(&mut next, 50_000),
                    (over, over),
                    "{case}: replayed to the next monitor"
                );
            }
        }
    }
}

/// A Stop kept by a build whose hooks did not say what they saw of the
/// record has no place and no view of it: it ends no turn. The open turn
/// it names is unknown, to the row and to recovery, and is not typed at.
#[tokio::test]
async fn an_unplaced_stop_kept_without_what_its_hook_saw_ends_no_turn() {
    let mut a = Reached::new(Tool::Claude);
    a.write(&[claude_prompt("p1")], 1000);
    std::fs::write(
        a.dir().join("turn.json"),
        include_str!("../../tests/fixtures/formats/turn-stopped-unobserved.json"),
    )
    .unwrap();
    let mut a = a.restart();
    a.round(Some(1000), 50_000, SETTLED);
    let kept = a.turns.state(&agent()).unwrap().stopped.clone().unwrap();
    assert_eq!(
        (kept.turn.as_deref(), kept.position, kept.read_to),
        (Some("p1"), None, None)
    );
    let seen = a.view(50_000);
    assert!(
        matches!(seen.state.activity, Activity::Unknown(_)),
        "{:?}",
        seen.state.activity
    );
    assert_eq!(
        seen.state.owed.as_ref().map(|o| &o.case),
        Some(&Case::HandsOff(HandsOff::BlindNudgeOff))
    );
    assert!(a.dispatched_at(50_000 + 180).is_empty());
    assert!(a.dispatched_at(50_000 + 10 * WARN).is_empty());
}
