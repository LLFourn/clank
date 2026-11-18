use super::*;
use crate::cli::monitor::fixtures::*;
use crate::cli::recovery::{Act, Answer, AttemptRef, Gate, Transport};
use crate::cli::watch_store::Producer;

/// Each kind of operation contention leaves unsettled is seen so: a
/// hosted timer's step, a client's serve, recovery's fold.
#[tokio::test]
async fn every_unsettled_operation_is_seen_unsettled() {
    let dir = tempfile::tempdir().unwrap();
    let mut none = std::collections::BTreeMap::new();
    let driver =
        || crate::cli::recovery::Driver::new(dir.path().to_path_buf(), "/nonexistent".into());
    assert!(!unavailable_in(&mut none, &mut Vec::new(), &mut driver()));

    let source = clank_core::agent_config::TimerSource {
        interrupt: false,
        interrupt_message: None,
        name: "once".into(),
        repeat_every: None,
        due_at: "1970-01-01T00:00:01Z".into(),
        content: None,
    };
    let key = source.source_key(None);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timers = crate::cli::timer_source::TimerSet::new(
        &[(
            clank_core::agent_config::WaitEventSource::Timer(source),
            crate::cli::event_source::SourceOrigin::Config,
        )],
        dir.path(),
        None,
        tx.into(),
    )
    .unwrap();
    let held = crate::cli::github::log::tests::hold(&[(dir.path(), &key)]);
    timers.due();
    held.release();
    let mut hosting = std::collections::BTreeMap::from([(
        Producer::Agent(AgentLabel::parse("claude").unwrap()),
        Hosted {
            timers: Some(timers),
            spec: Vec::new(),
            tasks: Default::default(),
        },
    )]);
    assert!(
        unavailable_in(&mut hosting, &mut Vec::new(), &mut driver()),
        "a timer's step"
    );

    let (mut served, _queued) = client(
        Served::Agent {
            author: AgentLabel::parse("claude").unwrap(),
        },
        "session",
    );
    served.unavailable = true;
    assert!(
        unavailable_in(&mut none, &mut vec![served], &mut driver()),
        "a client's serve"
    );

    let mut recovery = driver();
    recovery.reconcile(
        crate::cli::recovery::Observed {
            facts: Vec::new(),
            unknown: [AgentLabel::parse("claude").unwrap()].into(),
        },
        0,
    );
    assert!(
        unavailable_in(&mut none, &mut Vec::new(), &mut recovery),
        "recovery's fold"
    );
}

fn client(who: Served, session: &str) -> (Client, tokio::sync::mpsc::Receiver<String>) {
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    (
        Client {
            who,
            out,
            writer: None,
            holder: Some(Holder {
                tool: clank_core::Tool::OpenCode,
                session: session.into(),
            }),
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        },
        queued,
    )
}

fn attempt() -> AttemptRef {
    AttemptRef {
        session: "session-one".into(),
        opened_by: "t1".into(),
        n: 1,
    }
}

fn dispatch(author: &AgentLabel) -> Act {
    Act::Dispatch {
        author: author.clone(),
        attempt: attempt(),
        transport: Transport::Helper,
        tag: "tag-1".into(),
        target: "t1".into(),
        text: crate::cli::recovery::wake_text("tag-1"),
    }
}

const STARTED: &str = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}"#;
const DONE: &str =
    r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"T","error":null}}"#;
/// Codex's empty input on its band, between blank band rows, above its
/// footer.
const CODEX_EMPTY: &str = "• done\n\n\u{1b}[48;5;236m   \u{1b}[49m\n\u{1b}[48;5;236m›    \u{1b}[49m\n\u{1b}[48;5;236m   \u{1b}[49m\n\u{1b}[m  ? for shortcuts";

fn append(path: &std::path::Path, line: &str) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

/// A repo whose codex master is bound, auto, idle and owing work (an
/// event in its inbox), its first nudge claimed by the driver: what a
/// typed send's last look is made against.
struct Claimed {
    repo: tempfile::TempDir,
    codex: AgentLabel,
    record: std::path::PathBuf,
    driver: crate::cli::recovery::Driver,
    attempt: AttemptRef,
    /// The owed event, by its row.
    owed: (u64, String),
}

impl Claimed {
    fn new() -> Self {
        Self::with(Transport::Typed)
    }

    /// Claimed for `transport`.
    fn with(transport: Transport) -> Self {
        crate::cli::recovery::tests::enable(clank_core::Tool::Codex, Some(transport));
        Self::by_policy()
    }

    /// Claimed for the transport Codex's resume candidate gives it.
    fn by_policy() -> Self {
        use crate::cli::github::log::{EventLog, Transport as Wal};
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"codex":{"tool":"codex","role":"master"}}}"#,
        )
        .unwrap();
        let codex = AgentLabel::parse("codex").unwrap();
        let timer = clank_core::agent_config::TimerSource {
            interrupt: false,
            interrupt_message: None,
            name: "check".into(),
            repeat_every: None,
            due_at: "1970-01-01T00:01:40Z".into(),
            content: Some("scheduled work".into()),
        };
        crate::agent_store::update_agent_config(repo.path(), &codex, |c| {
            c.auto_mode = Some(clank_core::AutoMode::On);
            c.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
                timer.clone(),
            )];
        })
        .unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &codex,
            clank_core::Tool::Codex,
            &clank_core::ids::SessionId::parse("session-one").unwrap(),
        )
        .unwrap();
        let events = crate::agent_store::agents_root(repo.path()).join("codex/events");
        std::fs::create_dir_all(&events).unwrap();
        let mut log = EventLog::open(&events, &timer.source_key(None)).unwrap();
        let seq = log
            .append_inbox(
                Wal::Timer,
                Some("timer/100"),
                None,
                Some(100),
                &WaitItem::TimerEvent {
                    presentation: None,
                    name: "check".into(),
                    recurrence: None,
                    id: "timer/100".into(),
                    due_at: 100,
                    content: "scheduled work".into(),
                    selector: None,
                },
            )
            .unwrap();
        let ident = log
            .read_rows()
            .unwrap()
            .and_then(|read| read.rows.into_iter().find(|row| row.seq == seq))
            .unwrap()
            .ident;
        let record = repo.path().join("rollout.jsonl");
        append(&record, &STARTED.replace("\"T\"", "\"t1\""));
        append(&record, &DONE.replace("\"T\"", "\"t1\""));
        let mut driver = crate::cli::recovery::Driver::new(
            repo.path().to_path_buf(),
            std::path::PathBuf::from("/nonexistent-home"),
        );
        let mut claimed = Claimed {
            repo,
            codex,
            record,
            driver: crate::cli::recovery::Driver::new("/".into(), "/".into()),
            attempt: attempt(),
            owed: (seq, ident),
        };
        driver.reconcile(vec![claimed.facts()], 0);
        let acts = driver.due(10_000, |_, _| Gate::Go);
        let [Act::Dispatch { attempt, .. }] = acts.as_slice() else {
            panic!("{acts:?}");
        };
        claimed.attempt = attempt.clone();
        claimed.driver = driver;
        claimed
    }

    fn facts(&self) -> crate::cli::recovery::Facts {
        crate::cli::recovery::Facts {
            author: self.codex.clone(),
            agent_dir: crate::agent_store::agents_root(self.repo.path()).join("codex"),
            session: {
                let bound = crate::agent_store::binding(self.repo.path(), &self.codex)
                    .unwrap()
                    .unwrap();
                let mut session = serde_json::to_value(bound).unwrap();
                session["transcript"] = serde_json::json!(self.record);
                serde_json::from_value(session).unwrap()
            },
            auto: true,
            ledger: crate::cli::ledger::Ledger::owing_if(true),
            role: "master".into(),
            work: vec!["the timer".into()],
            blind_nudge: None,
            stall: None,
            attended: false,
            blocked_on: None,
            asking: false,
            projection: Default::default(),
        }
    }

    /// A transport's request after its preparation, the repo folded as
    /// it is now, and the receiver of the loop's answer.
    async fn ask(
        &self,
        screen: Option<(clank_core::Tool, Option<String>)>,
    ) -> (Authorize, tokio::sync::oneshot::Receiver<bool>) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let a = Authorize {
            author: self.codex.clone(),
            attempt: self.attempt.clone(),
            transport: match screen {
                Some(_) => Transport::Typed,
                None => Transport::CodexStart,
            },
            screen,
            state: folded(self.repo.path()).await.ok(),
            reply,
        };
        (a, answer)
    }

    /// The loop's answer to a typed send that read `screen`.
    async fn may_type(&mut self, clients: &mut Vec<Client>, screen: Option<&str>) -> bool {
        let typed = Some((clank_core::Tool::Codex, screen.map(str::to_string)));
        self.answer(clients, typed).await
    }

    /// The loop's answer to Codex's own `turn/start`, which reads no
    /// screen.
    async fn may_start(&mut self, clients: &mut Vec<Client>) -> bool {
        self.answer(clients, None).await
    }

    async fn answer(
        &mut self,
        clients: &mut Vec<Client>,
        screen: Option<(clank_core::Tool, Option<String>)>,
    ) -> bool {
        let (a, answer) = self.ask(screen).await;
        in_clients(clients, |clients| {
            authorize(
                self.repo.path(),
                clients,
                &Sources::hosting(Default::default()),
                &mut self.driver,
                &mut crate::cli::turn_custody::Turns::new(self.repo.path().to_path_buf()),
                a,
            )
        });
        answer.await.unwrap()
    }
}

/// The last look before typing is made from the live state on the loop
/// and the repo as it is on disk: every change already made by then,
/// folded or not, vetoes the send. What changes after it is the
/// acknowledged race.
#[tokio::test(flavor = "multi_thread")]
async fn any_change_before_the_last_look_vetoes_a_typed_send() {
    assert!(
        Claimed::new()
            .may_type(&mut Vec::new(), Some(CODEX_EMPTY))
            .await
    );
    assert!(
        Claimed::with(Transport::CodexStart)
            .may_start(&mut Vec::new())
            .await
    );
    assert!(
        !Claimed::new().may_start(&mut Vec::new()).await,
        "a typed claim is not sent by turn/start"
    );
    assert!(
        !Claimed::with(Transport::CodexStart)
            .may_type(&mut Vec::new(), Some(CODEX_EMPTY))
            .await,
        "nor a turn/start claim by typing"
    );
    for (route, change) in [Transport::Typed, Transport::CodexStart]
        .into_iter()
        .flat_map(|route| {
            [
                "draft",
                "no screen",
                "newer turn",
                "auto off",
                "rebound",
                "relaunched",
                "block",
                "off the roster",
                "role on disk",
                "work done on disk",
                "work cleared at a fold",
            ]
            .map(|change| (route, change))
        })
        .filter(|(route, change)| {
            *route == Transport::Typed || !matches!(*change, "draft" | "no screen")
        })
    {
        let mut c = Claimed::with(route);
        let mut clients: Vec<Client> = Vec::new();
        let mut screen = Some(CODEX_EMPTY.to_string());
        match change {
                "draft" => screen = Some(CODEX_EMPTY.replace("›    ", "› half")),
                "no screen" => screen = None,
                "newer turn" => append(&c.record, &STARTED.replace("\"T\"", "\"t2\"")),
                "auto off" => {
                    crate::agent_store::update_agent_config(c.repo.path(), &c.codex, |cfg| {
                        cfg.auto_mode = Some(clank_core::AutoMode::Off);
                    })
                    .unwrap();
                }
                "rebound" => {
                    crate::agent_store::bind_session_to_agent(
                        c.repo.path(),
                        &c.codex,
                        clank_core::Tool::Codex,
                        &clank_core::ids::SessionId::parse("session-two").unwrap(),
                    )
                    .unwrap();
                }
                "relaunched" => {
                    crate::agent_store::bind_session_to_agent(
                        c.repo.path(),
                        &c.codex,
                        clank_core::Tool::Codex,
                        &clank_core::ids::SessionId::parse("session-one").unwrap(),
                    )
                    .unwrap();
                }
                "block" => {
                    let blocks =
                        crate::agent_store::agents_root(c.repo.path()).join("codex/blocks");
                    std::fs::create_dir_all(&blocks).unwrap();
                    std::fs::write(blocks.join("which.md"), "Which way?").unwrap();
                }
                "off the roster" => std::fs::write(
                    c.repo.path().join(".clank/config.json"),
                    r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
                )
                .unwrap(),
                "role on disk" => std::fs::write(
                    c.repo.path().join(".clank/config.json"),
                    r#"{"agents":{"codex":{"tool":"codex","role":"commit"},"claude":{"tool":"claude","role":"master"}}}"#,
                )
                .unwrap(),
                "work done on disk" => {
                    let events = crate::agent_store::agents_root(c.repo.path()).join("codex/events");
                    let key = clank_core::agent_config::TimerSource {
                        interrupt: false,
                        interrupt_message: None,
                        name: "check".into(),
                        repeat_every: None,
                        due_at: "1970-01-01T00:01:40Z".into(),
                        content: Some("scheduled work".into()),
                    }
                    .source_key(None);
                    let mut log = crate::cli::github::log::EventLog::open(&events, &key).unwrap();
                    log.append_checked_ack(c.owed.0, &c.owed.1).unwrap();
                }
                _ => {
                    let f = crate::cli::recovery::Facts {
                        ledger: Default::default(),
                        ..c.facts()
                    };
                    c.driver.reconcile(vec![f], 10_001);
                }
            }
        let allowed = match route {
            Transport::Typed => c.may_type(&mut clients, screen.as_deref()).await,
            _ => c.may_start(&mut clients).await,
        };
        assert_eq!(
            allowed,
            change == "block",
            "{route:?}: {change}: a repo block never withholds a timer event"
        );
    }
}

/// A typist that finds the agent's pane in a fixed listing, through the
/// real lookup, reads `screen` there, and records what it would type
/// where.
struct Listed {
    scan: crate::cli::open_zellij::SessionScan,
    screen: Option<String>,
    /// What typing comes to.
    result: fn() -> Submitted,
    typed: std::sync::Mutex<Vec<(crate::cli::pane_target::Target, String)>>,
}

impl Typist for Listed {
    fn locate(
        &self,
        repo: std::path::PathBuf,
        author: AgentLabel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Located, String>> + Send>> {
        let found = crate::cli::pane_target::target_in(&self.scan, &repo, &author)
            .map(|target| (target, self.screen.clone()))
            .map_err(|e| format!("its pane was not found: {e:#}"));
        Box::pin(async move { found })
    }

    fn submit(
        &self,
        target: crate::cli::pane_target::Target,
        text: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Submitted> + Send>> {
        self.typed.lock().unwrap().push((target, text));
        Box::pin(std::future::ready((self.result)()))
    }
}

/// A pane as `list-panes --json --tab` reports it.
fn listed_pane(
    session: &str,
    id: u32,
    tab: u32,
    title: &str,
    command: Option<&str>,
) -> crate::cli::open_zellij::SessionPane {
    crate::cli::open_zellij::SessionPane {
        session: session.into(),
        pane: serde_json::from_value(serde_json::json!({
            "id": id,
            "title": title,
            "terminal_command": command,
            "tab_id": tab,
            "tab_name": format!("tab{tab}"),
            "exited": false,
        }))
        .unwrap(),
    }
}

/// Claimed's codex master, owed a nudge and found only by its name in a
/// listing of the repo's TUI pane in tab 0 of session `s` beside
/// `panes`, typing coming to `result`: what the typed send answered,
/// what it typed where, the agent's status line after, and its nudge
/// history.
async fn nudged_among(
    panes: Vec<crate::cli::open_zellij::SessionPane>,
    result: fn() -> Submitted,
) -> (
    Answer,
    Vec<(crate::cli::pane_target::Target, String)>,
    String,
    Vec<crate::cli::nudge_log::Record>,
) {
    let mut c = Claimed::by_policy();
    let tui = format!("clank tui --repo {}", c.repo.path().display());
    let mut listing = vec![listed_pane("s", 1, 0, "", Some(&tui))];
    listing.extend(panes);
    let typist = Listed {
        scan: crate::cli::open_zellij::SessionScan {
            panes: listing,
            unanswered: Vec::new(),
            listing_failed: false,
        },
        screen: Some(CODEX_EMPTY.to_string()),
        result,
        typed: Default::default(),
    };
    let Claimed {
        repo,
        codex,
        driver,
        attempt,
        ..
    } = &mut c;
    let (asks, mut heard) = tokio::sync::mpsc::unbounded_channel::<Authorize>();
    let sending = async {
        let answer = typed_send(
            repo.path().to_path_buf(),
            codex.clone(),
            attempt.clone(),
            "carry on".into(),
            &typist,
            &asks,
        )
        .await;
        drop(asks);
        answer
    };
    let looking = async {
        while let Some(a) = heard.recv().await {
            authorize(
                repo.path(),
                &Clients::new(),
                &Sources::hosting(Default::default()),
                driver,
                &mut crate::cli::turn_custody::Turns::new(repo.path().to_path_buf()),
                a,
            );
        }
    };
    let (answer, ()) = tokio::join!(sending, looking);
    driver.answer(codex, attempt, answer.clone(), 10_001);
    let agent_dir = crate::agent_store::agents_root(repo.path()).join("codex");
    let state = crate::cli::agent_state::load(&agent_dir).unwrap();
    let typed = typist.typed.into_inner().unwrap();
    let line = crate::cli::agent_state::line(&state, 10_001);
    (answer, typed, line, crate::cli::nudge_log::read(&agent_dir))
}

fn whole() -> Submitted {
    Ok(crate::cli::pane_input::Delivered::Whole)
}

/// What typing comes to reaches recovery as its answer and the nudge
/// history as its outcome: a known no-send as `Unavailable`, which
/// recovery retries; a prompt that may have landed as `Indeterminate`,
/// which it never replays; one recorded as `Submitted`.
#[tokio::test(flavor = "multi_thread")]
async fn a_typed_sends_outcome_reaches_recovery_and_the_history() {
    use crate::cli::nudge_log::Outcome;
    use crate::cli::open_zellij::PaneWrite;
    type Case = (fn() -> Submitted, Answer, fn(&Outcome) -> bool);
    let cases: [Case; 3] = [
        (
            || Err(PaneWrite::NotStarted("a draft was in the way".into())),
            Answer::Unavailable,
            |o| matches!(o, Outcome::Refused { .. }),
        ),
        (
            || Err(PaneWrite::Unknown("no record in time".into())),
            Answer::Indeterminate,
            |o| matches!(o, Outcome::Indeterminate { .. }),
        ),
        (whole, Answer::Submitted { started: None }, |o| {
            *o == Outcome::Submitted
        }),
    ];
    for (result, expected, logged) in cases {
        let (answer, typed, _, history) =
            nudged_among(vec![listed_pane("s", 3, 0, "codex", None)], result).await;
        assert_eq!(answer, expected);
        assert_eq!(typed.len(), 1, "{expected:?}");
        let [record] = history.as_slice() else {
            panic!("{expected:?}: {history:?}");
        };
        assert!(logged(&record.outcome), "{expected:?}: {record:?}");
    }
}

/// The incident: an idle master owing work, with no wait, that clank
/// did not start, is nudged into the pane its label names in the repo's
/// tab, whether that pane's title carries a status glyph or not.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_master_is_nudged_into_the_pane_named_by_its_label() {
    for title in ["codex", "💤 codex"] {
        let (answer, typed, shown, _) =
            nudged_among(vec![listed_pane("s", 3, 0, title, None)], whole).await;
        assert_eq!(answer, Answer::Submitted { started: None }, "{title}");
        let [(target, text)] = typed.as_slice() else {
            panic!("{title}: {typed:?}");
        };
        assert_eq!(
            (target.session.as_str(), target.pane.as_str(), target.tool),
            ("s", "terminal_3", clank_core::Tool::Codex),
            "{title}"
        );
        assert_eq!(text, "carry on");
        assert!(!shown.contains("not nudged"), "{title}: {shown}");
    }
}

/// No pane, two panes, or a pane named for the agent only in another
/// tab or session: nothing is typed, and the status line says why,
/// with no word of a launch or a version.
#[tokio::test(flavor = "multi_thread")]
async fn no_one_named_pane_is_no_nudge_and_status_says_why() {
    let cases = [
        (Vec::new(), "no pane named `codex`"),
        (
            vec![
                listed_pane("s", 3, 0, "codex", None),
                listed_pane("s", 4, 0, "💤 codex", None),
            ],
            "`codex` has 2 live panes",
        ),
        (
            vec![
                listed_pane("s", 3, 1, "codex", None),
                listed_pane("other", 3, 0, "codex", None),
            ],
            "no pane named `codex`",
        ),
    ];
    for (panes, why) in cases {
        let (answer, typed, shown, _) = nudged_among(panes, whole).await;
        assert!(
            matches!(&answer, Answer::NoPane(said) if said.contains(why)),
            "{why}: {answer:?}"
        );
        assert!(typed.is_empty(), "{why}: {typed:?}");
        assert!(
            shown.contains("not nudged: its pane was not found"),
            "{shown}"
        );
        assert!(shown.contains(why), "{shown}");
        for word in ["provenance", "launch", "version"] {
            assert!(!shown.contains(word), "{word}: {shown}");
        }
    }
}

/// Drafts already in the input at the last look, on the prompt row or
/// under blank rows of the input, are refused: a typed send writes only
/// on the loop's yes.
#[tokio::test(flavor = "multi_thread")]
async fn a_draft_in_the_input_is_never_written_over() {
    let band = |l: &str| format!("\u{1b}[48;5;236m{l}   \u{1b}[49m");
    let screen = |rows: &[&str]| {
        let rows: Vec<String> = [""]
            .iter()
            .chain(rows)
            .chain(&[""])
            .map(|l| band(l))
            .collect();
        format!("• done\n\n{}\n\u{1b}[m  ? for shortcuts", rows.join("\n"))
    };
    for draft in [
        screen(&["› half"]),
        screen(&["› ", "", "  unfinished draft"]),
        screen(&["› ", "  draft", ""]),
    ] {
        let mut c = Claimed::new();
        assert!(
            !c.may_type(&mut Vec::new(), Some(&draft)).await,
            "{draft:?}"
        );
    }
}

/// A transport may mutate only on the loop's yes: a no, or a loop that
/// is gone, is a refusal.
#[tokio::test(flavor = "multi_thread")]
async fn a_transport_waits_for_the_loops_yes() {
    let c = Claimed::new();
    for allow in [Some(true), Some(false), None] {
        let (asks, mut heard) = tokio::sync::mpsc::unbounded_channel::<Authorize>();
        let loop_side = tokio::spawn(async move {
            let a = heard.recv().await.unwrap();
            if let Some(allow) = allow {
                let _ = a.reply.send(allow);
            }
        });
        let go = authorized(
            &asks,
            c.repo.path(),
            c.codex.clone(),
            c.attempt.clone(),
            None,
            Transport::CodexStart,
        )
        .await;
        loop_side.await.unwrap();
        assert_eq!(go, allow == Some(true), "{allow:?}");
    }
}

/// A recurring occurrence acknowledged on disk is no longer owed at the
/// last look, though the monitor's own backlog would still list it
/// until its next invalidation: the look reads the backlog afresh.
#[tokio::test(flavor = "multi_thread")]
async fn an_acknowledged_recurring_occurrence_is_not_owed_at_the_last_look() {
    use crate::cli::github::log::EventLog;
    let mut c = Claimed::new();
    let events = crate::agent_store::agents_root(c.repo.path()).join("codex/events");
    let mut timer = clank_core::agent_config::TimerSource {
        interrupt: false,
        interrupt_message: None,
        name: "check".into(),
        repeat_every: None,
        due_at: "1970-01-01T00:01:40Z".into(),
        content: Some("scheduled work".into()),
    };
    EventLog::open(&events, &timer.source_key(None))
        .unwrap()
        .append_checked_ack(c.owed.0, &c.owed.1)
        .unwrap();
    timer.name = "recurring".into();
    timer.repeat_every = Some("60s".into());
    let r = crate::cli::timer_source::recurrence::Recurrence::parse(&timer, None).unwrap();
    let mut log = EventLog::open(&events, &r.source_key()).unwrap();
    log.accept_recurring(&r, "recurring work", 100).unwrap();
    let row = log.read_rows().unwrap().unwrap().rows[0].clone();
    assert!(c.may_type(&mut Vec::new(), Some(CODEX_EMPTY)).await, "owed");
    log.append_checked_ack(row.seq, &row.ident).unwrap();
    assert!(
        !c.may_type(&mut Vec::new(), Some(CODEX_EMPTY)).await,
        "acknowledged"
    );
}

fn records() -> crate::cli::recovery::RecordWatch {
    crate::cli::recovery::RecordWatch::spawn(tokio::sync::mpsc::unbounded_channel().0)
}

/// The authorized attempt reaches that session's helper and nobody
/// else: not the agent's ordinary wait, and not a helper for another
/// session of the same agent.
#[tokio::test]
async fn a_dispatch_reaches_only_that_sessions_helper() {
    let repo = tempfile::tempdir().unwrap();
    let author = AgentLabel::parse("opencode").unwrap();
    let (agent, mut agent_rx) = client(
        Served::Agent {
            author: author.clone(),
        },
        "session-one",
    );
    let (stale, mut stale_rx) = client(
        Served::Helper {
            author: author.clone(),
        },
        "session-two",
    );
    let (helper, mut helper_rx) = client(
        Served::Helper {
            author: author.clone(),
        },
        "session-one",
    );
    let clients = Clients::of(vec![agent, stale, helper]);
    let (tx, mut answers) = tokio::sync::mpsc::unbounded_channel();
    let mut owned = OwnedTasks::default();
    act_on_recovery(
        repo.path(),
        &clients,
        vec![dispatch(&author)],
        &tx,
        &tokio::sync::mpsc::unbounded_channel().0,
        &records(),
        &mut owned,
    );

    let note: Note = serde_json::from_str(helper_rx.try_recv().unwrap().trim_end()).unwrap();
    assert_eq!(
        note,
        Note::Recover {
            opened_by: "t1".into(),
            n: 1,
            tag: "tag-1".into(),
            wake: crate::cli::recovery::wake_text("tag-1"),
        }
    );
    assert!(
        agent_rx.try_recv().is_err(),
        "an ordinary wait is never handed a retry"
    );
    assert!(stale_rx.try_recv().is_err(), "nor another session's helper");
    assert!(
        answers.try_recv().is_err(),
        "a delivered dispatch answers through its helper"
    );

    // A monitor on a replaced build hands the same helper nothing:
    // the attempt is answered as a transport that was not there.
    {
        use crate::upgrade::tests::{AsProcess, Installed};
        let installed = Installed::current();
        let authority = installed.authority();
        let _as = AsProcess::of(&authority);
        installed.replace(1);
        act_on_recovery(
            repo.path(),
            &clients,
            vec![dispatch(&author)],
            &tx,
            &tokio::sync::mpsc::unbounded_channel().0,
            &records(),
            &mut owned,
        );
        assert!(
            helper_rx.try_recv().is_err(),
            "a stale monitor sent a retry"
        );
        assert_eq!(
            answers.try_recv().unwrap(),
            (author.clone(), attempt(), Answer::Unavailable)
        );
    }

    act_on_recovery(
        repo.path(),
        &Clients::new(),
        vec![dispatch(&author)],
        &tx,
        &tokio::sync::mpsc::unbounded_channel().0,
        &records(),
        &mut owned,
    );
    assert_eq!(
        answers.try_recv().unwrap(),
        (author, attempt(), Answer::Unavailable)
    );
}

/// A typed nudge is not typed by a monitor on a replaced build: the
/// pane is not even looked for.
#[tokio::test]
async fn a_stale_monitor_types_no_nudge() {
    use crate::upgrade::tests::{AsProcess, Installed};
    let c = Claimed::by_policy();
    let typist = Listed {
        scan: crate::cli::open_zellij::SessionScan {
            panes: Vec::new(),
            unanswered: Vec::new(),
            listing_failed: false,
        },
        screen: Some(CODEX_EMPTY.to_string()),
        result: || Ok(crate::cli::pane_input::Delivered::Whole),
        typed: Default::default(),
    };
    let (asks, _heard) = tokio::sync::mpsc::unbounded_channel::<Authorize>();
    let send = || {
        typed_send(
            c.repo.path().to_path_buf(),
            c.codex.clone(),
            c.attempt.clone(),
            "carry on".into(),
            &typist,
            &asks,
        )
    };
    assert!(
        matches!(send().await, Answer::NoPane(_)),
        "on its own build it looks for the pane, which this listing lacks"
    );
    let installed = Installed::current();
    let authority = installed.authority();
    let _as = AsProcess::of(&authority);
    installed.replace(1);
    assert_eq!(send().await, Answer::Unavailable);
    assert!(typist.typed.lock().unwrap().is_empty());
}

/// A helper is kept alive with beats and is never handed inbox work,
/// so its presence is never mistaken for an agent waiting for some.
#[tokio::test]
async fn a_helper_is_served_beats_never_work() {
    let repo = tempfile::tempdir().unwrap();
    let author = AgentLabel::parse("opencode").unwrap();
    let held = WaitItem::CommandEvent {
        presentation: None,
        source_cwd: None,
        name: "x".into(),
        id: "x".into(),
        event: "x".into(),
        content: "work".into(),
        at: None,
        failure: false,
        requires_reply: false,
        reply_route: None,
        selector: None,
    };
    let mut sources = holding(vec![(author.clone().into(), held)]);
    let (mut helper, mut helper_rx) = client(
        Served::Helper {
            author: author.clone(),
        },
        "session-one",
    );
    helper.holder = None;
    let mut clients = vec![helper];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let _ = serve_to(repo.path(), &mut clients, &state, None, &mut sources);
    let note: Note = serde_json::from_str(helper_rx.try_recv().unwrap().trim_end()).unwrap();
    assert_eq!(note, Note::Beat);
    assert_eq!(clients.len(), 1, "it stays parked");
    assert_eq!(
        sources.pending().len(),
        1,
        "the work is still held for a deliverer"
    );

    // A helper for a session that does not own the agent is refused,
    // like any other client of a superseded session.
    let (stale, mut stale_rx) = client(
        Served::Helper {
            author: author.clone(),
        },
        "session-one",
    );
    let mut clients = vec![stale];
    let _ = serve_to(repo.path(), &mut clients, &state, None, &mut sources);
    let note: Note = serde_json::from_str(stale_rx.try_recv().unwrap().trim_end()).unwrap();
    assert!(matches!(note, Note::Refused { .. }), "{note:?}");
    assert!(clients.is_empty());
}

/// Obligation is what a newly arriving waiter would be handed, a
/// deliverer is an ordinary wait, and a recovery helper is not one.
#[tokio::test(flavor = "multi_thread")]
async fn facts_name_who_owes_work_and_who_is_waiting_for_it() {
    use crate::cli::github::log::{EventLog, Transport as Wal};
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
    std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#,
        )
        .unwrap();
    let codex = AgentLabel::parse("codex").unwrap();
    let timer = clank_core::agent_config::TimerSource {
        interrupt: false,
        interrupt_message: None,
        name: "check".into(),
        repeat_every: None,
        due_at: "1970-01-01T00:01:40Z".into(),
        content: Some("scheduled work".into()),
    };
    crate::agent_store::update_agent_config(repo.path(), &codex, |config| {
        // Auto's builtin default is off; without this the outcome
        // follows the runner's ~/.clank/config.json.
        config.auto_mode = Some(clank_core::AutoMode::On);
        config.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
            timer.clone(),
        )];
    })
    .unwrap();
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &codex,
        clank_core::Tool::Codex,
        &clank_core::ids::SessionId::parse("session-one").unwrap(),
    )
    .unwrap();
    let events = crate::agent_store::agents_root(repo.path()).join("codex/events");
    std::fs::create_dir_all(&events).unwrap();
    let mut log = EventLog::open(&events, &timer.source_key(None)).unwrap();
    let seq = log
        .append_inbox(
            Wal::Timer,
            Some("timer/100"),
            None,
            Some(100),
            &WaitItem::TimerEvent {
                presentation: None,
                name: "check".into(),
                recurrence: None,
                id: "timer/100".into(),
                due_at: 100,
                content: "scheduled work".into(),
                selector: None,
            },
        )
        .unwrap();
    let state = folded(repo.path()).await.unwrap();
    let turns = std::cell::RefCell::new(crate::cli::turn_custody::Turns::new(
        repo.path().to_path_buf(),
    ));
    let facts = || {
        in_turns(&mut turns.borrow_mut(), |custody| {
            custody.facts(
                repo.path(),
                &state,
                &mut Sources::hosting(Default::default()),
            )
        })
        .facts
    };

    let alone = facts();
    assert_eq!(
        alone.len(),
        1,
        "an agent with no bound session is not considered"
    );
    let owed = alone[0].ledger.owed();
    assert_eq!(owed.len(), 1, "{alone:?}");
    assert_eq!(
        (
            owed[0].owed_since,
            owed[0].entry.delivery,
            owed[0].entry.what.as_str()
        ),
        (
            crate::cli::ledger::Since::At(log.read_rows().unwrap().unwrap().rows[0].at),
            crate::cli::ledger::Delivery::NotDelivered,
            "timer check"
        ),
        "owed since its log row was written, and delivered to nobody"
    );
    assert_eq!(alone[0].blind_nudge, None, "off unless configured");
    assert!(!alone[0].attended);
    // A question from a session no longer the agent's is over at the
    // next fold.
    let qdir = crate::agent_store::agents_root(repo.path()).join("codex");
    let bind = |tool, session| {
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &codex,
            tool,
            &clank_core::ids::SessionId::parse(session).unwrap(),
        )
        .unwrap()
    };
    bind(clank_core::Tool::Claude, "session-zero");
    crate::cli::question_state::observe(
        &qdir,
        [crate::cli::question_state::Observation::Asked {
            key: crate::cli::question_state::Key {
                session: "session-zero".into(),
                prompt: "p".into(),
                tool_use: "t".into(),
            },
            header: "old".into(),
            input: "h".into(),
        }],
    );
    assert!(facts()[0].asking, "its record is read for what ends it");
    bind(clank_core::Tool::Codex, "session-one");
    facts();
    assert!(
        crate::cli::question_state::load(&qdir, "session-one")
            .unwrap()
            .pending
            .is_empty()
    );

    // A count that still describes the agent survives folds, a standing
    // block included; a change of session or of block clears it.
    crate::cli::block::asked(repo.path(), "codex", "mic", "Which microphone?");
    let owed_now = || {
        Sources::hosting(Default::default())
            .owed_now(repo.path(), &state, &codex)
            .expect("no log is held")
    };
    let owed = owed_now();
    assert!(
        identities(&owed).iter().any(|id| id.starts_with("blocked")) && owed.len() > 1,
        "the block and the event it does not withhold: {owed:?}"
    );
    let count = || {
        turns
            .borrow()
            .state(&codex)
            .and_then(|s| s.repeat.as_ref())
            .map(|r| r.count)
    };
    let deliver = |session: &str, n: u64| {
        let report = crate::cli::turn_wire::Report::TurnEnd {
            stop: clank_core::turn_policy::TurnEndReport {
                tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
                hook_started: None,
                tool: clank_core::Tool::Codex,
                session: clank_core::SessionId::parse(session).unwrap(),
                place: clank_core::turn_policy::TurnPlace {
                    turn: None,
                    position: Some(n),
                    human_turn: false,
                    read_to: None,
                },
            },
            deadline: u64::MAX,
            questions: Vec::new(),
        };
        let g =
            crate::cli::turn_custody::Gathered::of(repo.path(), &codex, Some((&owed_now(), None)));
        let frame = turns
            .borrow_mut()
            .answer(&codex, n, &report, true, &g)
            .expect("folded");
        let presentation = frame.presentation().expect("a delivery").clone();
        turns
            .borrow_mut()
            .result(
                &codex,
                n,
                crate::cli::turn_wire::TurnResult::Delivered { presentation },
                &g,
            )
            .unwrap();
    };
    for n in 1..=4 {
        deliver("session-one", n);
        facts();
    }
    assert_eq!(count(), Some(4));
    facts();
    assert_eq!(count(), Some(4), "an unchanged fold keeps the count");
    std::fs::remove_file(repo.path().join(".clank/blocks/mic.md")).unwrap();
    facts();
    assert_eq!(count(), None, "the block was answered");
    bind(clank_core::Tool::Codex, "session-two");
    facts();
    assert_eq!(count(), None, "another session's count");
    bind(clank_core::Tool::Codex, "session-one");

    // An attendance declared in the turn a delivery started holds
    // recovery back over the work that delivery received.
    let g = crate::cli::turn_custody::Gathered::of(repo.path(), &codex, Some((&owed_now(), None)));
    let stop = crate::cli::turn_wire::Report::TurnEnd {
        stop: clank_core::turn_policy::TurnEndReport {
            tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
            hook_started: None,
            tool: clank_core::Tool::Codex,
            session: clank_core::SessionId::parse("session-one").unwrap(),
            place: Default::default(),
        },
        deadline: u64::MAX,
        questions: Vec::new(),
    };
    let frame = turns
        .borrow_mut()
        .answer(&codex, 99, &stop, true, &g)
        .unwrap();
    let presentation = frame
        .presentation()
        .unwrap_or_else(|| panic!("owed work is delivered: {frame:?}"))
        .clone();
    turns
        .borrow_mut()
        .result(
            &codex,
            99,
            crate::cli::turn_wire::TurnResult::Delivered { presentation },
            &g,
        )
        .unwrap();
    assert!(
        !facts()[0].attended,
        "received work with nothing attended does not hold recovery back"
    );
    turns
        .borrow_mut()
        .attend(
            &codex,
            clank_core::turn_policy::AttendRequest::Declare {
                session: clank_core::SessionId::parse("session-one").unwrap(),
                name: "sleeper".into(),
                desc: None,
                expect: 600_000,
            },
            &g,
        )
        .unwrap();
    assert!(
        facts()[0].attended,
        "a declaration holds recovery back over received work, for codex too"
    );
    assert!(!facts()[0].projection.attended.is_empty(), "and is shown");
    bind(clank_core::Tool::Codex, "session-three");
    assert!(
        !facts()[0].attended,
        "a new binding drops the old one's attendance"
    );
    bind(clank_core::Tool::Codex, "session-one");

    let ident = log.read_rows().unwrap().unwrap().rows[0].ident.clone();
    log.append_checked_ack(seq, &ident).unwrap();
    let state = folded(repo.path()).await.unwrap();
    let handled = in_turns(&mut turns.borrow_mut(), |custody| {
        custody.facts(
            repo.path(),
            &state,
            &mut Sources::hosting(Default::default()),
        )
    })
    .facts;
    assert!(
        handled[0].ledger.owed().is_empty(),
        "handled work owes nothing: {handled:?}"
    );
}

/// Binding writes no `config.json`, so an agent on the user's default
/// has none: its binding alone says whose the attempt is, and its auto
/// mode is inherited.
#[test]
fn a_binding_without_agent_config_is_its_session_and_inherits_auto_mode() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let codex = AgentLabel::parse("codex").unwrap();
    let mut turns = crate::cli::turn_custody::Turns::new(repo.path().to_path_buf());
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &codex,
        clank_core::Tool::Codex,
        &clank_core::ids::SessionId::parse("session-one").unwrap(),
    )
    .unwrap();
    assert!(
        !crate::agent_store::agent_config_path(repo.path(), &codex).exists(),
        "binding wrote no config"
    );
    let mut gate = |home: &std::path::Path| {
        recovery_gate_in(
            repo.path(),
            Some(home),
            &Clients::new(),
            &mut turns,
            &codex,
            "session-one",
            &[],
        )
    };
    assert_eq!(gate(home.path()), Gate::AutoOff, "the builtin default");
    crate::cli::team::mutate_user_config(home.path(), |c| {
        c.auto = Some(clank_core::AutoMode::On);
        Ok(())
    })
    .unwrap();
    assert_eq!(gate(home.path()), Gate::Go, "the user's default");
}

/// A due attempt is decided against the agent as it is now: its
/// binding, its deliverer, its auto mode and its attendance.
#[test]
fn the_gate_reads_the_agent_as_it_is_now() {
    let repo = tempfile::tempdir().unwrap();
    let codex = AgentLabel::parse("codex").unwrap();
    let turns = std::cell::RefCell::new(crate::cli::turn_custody::Turns::new(
        repo.path().to_path_buf(),
    ));
    let gate_owing = |clients: Vec<Client>, session: &str, owed: &[WaitItem]| {
        recovery_gate(
            repo.path(),
            &Clients::of(clients),
            &mut turns.borrow_mut(),
            &codex,
            session,
            owed,
        )
    };
    let gate_for = |clients: Vec<Client>, session: &str| gate_owing(clients, session, &[]);
    let gate = |clients: Vec<Client>| gate_for(clients, "session-one");
    assert_eq!(gate(vec![]), Gate::SessionChanged, "never bound");
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &codex,
        clank_core::Tool::Codex,
        &clank_core::ids::SessionId::parse("session-one").unwrap(),
    )
    .unwrap();
    crate::agent_store::set_auto_mode(repo.path(), &codex, clank_core::AutoMode::On).unwrap();
    assert_eq!(gate(vec![]), Gate::Go);
    assert_eq!(gate_for(vec![], "session-two"), Gate::SessionChanged);

    let (waiter, _rx) = client(
        Served::Agent {
            author: codex.clone(),
        },
        "session-one",
    );
    assert_eq!(
        gate(vec![waiter]),
        Gate::Go,
        "a wait that is registered and holds nothing of the work is not delivering it"
    );

    crate::agent_store::set_auto_mode(repo.path(), &codex, clank_core::AutoMode::Off).unwrap();
    assert_eq!(gate(vec![]), Gate::AutoOff);

    // A declaration holds recovery from the moment it is made.
    crate::agent_store::set_auto_mode(repo.path(), &codex, clank_core::AutoMode::On).unwrap();
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &codex,
        clank_core::Tool::Claude,
        &clank_core::ids::SessionId::parse("session-one").unwrap(),
    )
    .unwrap();
    assert_eq!(gate(vec![]), Gate::Go);
    let g = crate::cli::turn_custody::Gathered::of(repo.path(), &codex, Some((&[], None)));
    turns
        .borrow_mut()
        .attend(
            &codex,
            clank_core::turn_policy::AttendRequest::Declare {
                session: clank_core::SessionId::parse("session-one").unwrap(),
                name: "sleeper".into(),
                desc: None,
                expect: 600_000,
            },
            &g,
        )
        .unwrap();
    assert_eq!(gate(vec![]), Gate::Hold);
    turns
        .borrow_mut()
        .attend(
            &codex,
            clank_core::turn_policy::AttendRequest::Clear {
                session: clank_core::SessionId::parse("session-one").unwrap(),
                which: clank_core::turn_policy::Clearing::All,
            },
            &g,
        )
        .unwrap();
    assert_eq!(gate(vec![]), Gate::Go);

    // A question pending in the bound session holds recovery, with no
    // viewer anywhere; its answer releases it.
    let dir = crate::agent_store::agents_root(repo.path()).join("codex");
    let key = crate::cli::question_state::Key {
        session: "session-one".into(),
        prompt: "p1".into(),
        tool_use: "t1".into(),
    };
    crate::cli::question_state::observe(
        &dir,
        [crate::cli::question_state::Observation::Asked {
            key: key.clone(),
            header: "Which?".into(),
            input: "h".into(),
        }],
    );
    assert_eq!(gate(vec![]), Gate::Hold);
    crate::cli::question_state::observe(
        &dir,
        [crate::cli::question_state::Observation::Result { key }],
    );
    assert_eq!(gate(vec![]), Gate::Go);

    // Every repository block holds an otherwise idle agent; unrelated
    // work still wakes it regardless of who asked the question.
    let ask = |who: &str, name: &str, q: &str| crate::cli::block::asked(repo.path(), who, name, q);
    ask("claude", "stork", "Which take?");
    assert!(crate::cli::wait::blocked(repo.path(), &codex));
    assert_eq!(
        gate(vec![]),
        Gate::Hold,
        "the repository waits for the human"
    );
    ask("codex", "mine", "Which way?");
    assert_eq!(gate(vec![]), Gate::Hold, "its own block");
}

/// A Codex standby `artist`, bound and in auto, in a repo whose master
/// `claude` has a block pending: its record, and the turns it is
/// folded with.
struct Standby {
    repo: tempfile::TempDir,
    artist: AgentLabel,
    record: std::path::PathBuf,
    turns: crate::cli::turn_custody::Turns,
}

impl Standby {
    fn under_the_masters_block() -> Self {
        crate::cli::recovery::tests::enable(clank_core::Tool::Codex, Some(Transport::Typed));
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
                repo.path().join(".clank/config.json"),
                r#"{"agents":{"claude":{"tool":"claude","role":"master"},"artist":{"tool":"codex","role":"standby"}}}"#,
            )
            .unwrap();
        let artist = AgentLabel::parse("artist").unwrap();
        crate::agent_store::update_agent_config(repo.path(), &artist, |c| {
            c.auto_mode = Some(clank_core::AutoMode::On);
        })
        .unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &artist,
            clank_core::Tool::Codex,
            &clank_core::ids::SessionId::parse("session-one").unwrap(),
        )
        .unwrap();
        crate::cli::block::asked(repo.path(), "claude", "stork-retakes", "Which takes?");
        let record = repo.path().join("rollout.jsonl");
        append(&record, &STARTED.replace("\"T\"", "\"t1\""));
        let turns = crate::cli::turn_custody::Turns::new(repo.path().to_path_buf());
        Standby {
            repo,
            artist,
            record,
            turns,
        }
    }

    fn message(&self) {
        crate::inbox::deliver(
            self.repo.path(),
            &self.artist,
            Some(&crate::inbox::Sender {
                label: "claude".into(),
                repo: self.repo.path().to_path_buf(),
            }),
            "2026-11-10T01:00:00Z",
            "the retakes are in",
        )
        .unwrap();
    }

    fn dir(&self) -> std::path::PathBuf {
        crate::agent_store::agents_root(self.repo.path()).join("artist")
    }

    /// What a fold of the repo as it is now tells recovery of `artist`,
    /// reading the record this fixture wrote.
    async fn facts(&mut self) -> crate::cli::recovery::Facts {
        let state = folded(self.repo.path()).await.unwrap();
        let mut facts = in_turns(&mut self.turns, |custody| {
            custody.facts(
                self.repo.path(),
                &state,
                &mut Sources::hosting(Default::default()),
            )
        })
        .facts
        .into_iter()
        .find(|f| f.author == self.artist)
        .unwrap();
        facts.session.transcript = Some(self.record.display().to_string());
        facts
    }

    /// The deadline's own gate, against the repo folded now.
    async fn due(&mut self, driver: &mut crate::cli::recovery::Driver, now: u64) -> Vec<Act> {
        let state = folded(self.repo.path()).await.unwrap();
        let (repo, turns) = (self.repo.path(), &mut self.turns);
        driver.due(now, |author, session| {
            due_gate(
                repo,
                &Clients::new(),
                turns,
                Some(&state),
                &mut Sources::hosting(Default::default()),
                author,
                session,
            )
        })
    }

    fn shown(&self, now: u64) -> crate::cli::agent_state::View {
        crate::cli::agent_state::View::at(
            crate::cli::agent_state::load(&self.dir()).unwrap(),
            None,
            now,
        )
    }
}

/// Date `record`'s last write.
fn written_at(record: &std::path::Path, secs: u64) {
    std::fs::File::options()
        .write(true)
        .open(record)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .unwrap();
}

fn dispatched(acts: &[Act]) -> usize {
    acts.iter()
        .filter(|a| matches!(a, Act::Dispatch { .. }))
        .count()
}

/// 2026-11-10, `artist4`: a message owed to a standby whose turn has
/// sat open and silent since the day before, no waiter, and the
/// master's block pending. The block is not the standby's and
/// withholds nothing a message is, so the standby is owed the message
/// from its arrival: its row shouts it once it has gone undelivered
/// for the stall window, never says `working`, and it is nudged once
/// when the window and the nudge's delay have passed.
#[tokio::test]
async fn an_owed_message_under_another_agents_block_is_shouted_then_nudged() {
    use crate::cli::agent_state::{Active, Case};
    use crate::cli::ledger::{Delivery, Since};
    let mut s = Standby::under_the_masters_block();
    s.message();
    written_at(&s.record, 1000);
    assert!(
        crate::cli::wait::blocked(s.repo.path(), &s.artist),
        "the master's block is repo-wide"
    );

    let f = s.facts().await;
    assert!(
        !f.attended && f.blocked_on.is_none() && !f.asking,
        "another agent's block is no reason to keep hands off: {f:?}"
    );
    let owed = f.ledger.owed();
    let [message] = owed.as_slice() else {
        panic!("{owed:?}");
    };
    assert_eq!(message.entry.what, "message from claude");
    assert_eq!(message.entry.delivery, Delivery::NotDelivered);
    let Since::At(arrived) = message.owed_since else {
        panic!("the message's log row dates it: {message:?}");
    };
    let stall = f.stall.expect("the stall watchdog is on by default");
    let (warn, nudge) = (stall.warn, stall.nudge.unwrap());

    let mut d = crate::cli::recovery::Driver::new(
        s.repo.path().to_path_buf(),
        std::path::PathBuf::from("/nonexistent-home"),
    );
    assert_eq!(dispatched(&d.reconcile(vec![f.clone()], arrived)), 0);
    assert_eq!(
        d.next_due(),
        Some(arrived + warn),
        "silent since the message arrived, not since yesterday's last write"
    );
    let early = s.shown(arrived + warn - 1);
    assert_eq!(
        early.state.owed.as_ref().map(|o| &o.case),
        Some(&Case::Active(Active::Turn)),
        "an open turn inside the stall window"
    );
    assert_eq!(early.undelivered(arrived + warn - 1), None);

    assert_eq!(dispatched(&s.due(&mut d, arrived + warn).await), 0);
    let late = s.shown(arrived + warn);
    assert_eq!(
        late.line(arrived + warn),
        "UNDELIVERED 40m · message from claude · silent 40m — nudging in 5m"
    );
    assert!(
        !late.working(arrived + warn),
        "a silent open turn is not working"
    );
    assert_eq!(late.owed.as_ref().unwrap().age, Some(warn));

    assert_eq!(
        dispatched(&s.due(&mut d, arrived + warn + nudge - 1).await),
        0
    );
    let acts = s.due(&mut d, arrived + warn + nudge).await;
    let [
        Act::Dispatch {
            attempt, text, tag, ..
        },
    ] = acts.as_slice()
    else {
        panic!("nudged once the window has passed: {acts:?}");
    };
    assert_eq!(text, &crate::cli::recovery::stall_text(tag));

    // One owner: the same silence, folded and looked at again and
    // again, is nudged by nothing else.
    let attempt = attempt.clone();
    d.answer(
        &s.artist,
        &attempt,
        Answer::Submitted { started: None },
        arrived + warn + nudge + 1,
    );
    for later in [1, 60, 3600, 86_400] {
        let now = arrived + warn + nudge + 1 + later;
        let f = s.facts().await;
        assert_eq!(dispatched(&d.reconcile(vec![f], now)), 0, "{later}");
        assert_eq!(dispatched(&s.due(&mut d, now).await), 0, "{later}");
    }
}

/// The same standby, bound and never run: its record is there and
/// shows no turn. It is owed the message all the same, so it is an idle
/// agent at its prompt, nudged once its settle time has passed, under
/// the deadline's own gate and the master's block.
#[tokio::test]
async fn an_owed_message_to_an_agent_that_never_ran_is_nudged_at_its_prompt() {
    use crate::cli::agent_state::Case;
    let mut s = Standby::under_the_masters_block();
    std::fs::write(&s.record, "").unwrap();
    s.message();
    let f = s.facts().await;
    let now = crate::cli::recovery::unix_now();
    let mut d = crate::cli::recovery::Driver::new(
        s.repo.path().to_path_buf(),
        std::path::PathBuf::from("/nonexistent-home"),
    );
    assert_eq!(dispatched(&d.reconcile(vec![f], now)), 0);
    assert_eq!(
        s.shown(now).state.owed.as_ref().map(|o| &o.case),
        Some(&Case::Recovering),
        "no turn to read is no reason to leave it"
    );
    let due = d.next_due().expect("a nudge is scheduled");
    assert_eq!(dispatched(&s.due(&mut d, due - 1).await), 0);
    let acts = s.due(&mut d, due).await;
    let [
        Act::Dispatch {
            text, tag, target, ..
        },
    ] = acts.as_slice()
    else {
        panic!("nudged at its prompt: {acts:?}");
    };
    assert_eq!(text, &crate::cli::recovery::nudge_text(tag));
    assert_eq!(target, crate::cli::recovery::AT_PROMPT);
}

/// A pending block that is not the agent's makes nothing owed to it:
/// with no event of its own it has no row to shout and no nudge. A
/// message is owed, block or not.
#[tokio::test]
async fn a_block_alone_owes_an_agent_nothing_and_a_message_is_still_owed() {
    let mut s = Standby::under_the_masters_block();
    written_at(&s.record, 1000);
    let quiet = s.facts().await;
    assert!(quiet.ledger.owed().is_empty(), "{quiet:?}");
    let mut d = crate::cli::recovery::Driver::new(
        s.repo.path().to_path_buf(),
        std::path::PathBuf::from("/nonexistent-home"),
    );
    let now = crate::cli::recovery::unix_now();
    assert_eq!(dispatched(&d.reconcile(vec![quiet], now)), 0);
    assert_eq!(d.next_due(), None, "nothing owed schedules nothing");
    assert_eq!(s.shown(now + 86_400).state.owed, None);
    assert_eq!(s.shown(now + 86_400).undelivered(now + 86_400), None);

    s.message();
    let owing = s.facts().await;
    assert_eq!(owing.ledger.owed().len(), 1);
    d.reconcile(vec![owing], now);
    assert!(d.next_due().is_some(), "the message is owed, block or not");
}

/// A delivery is proven to the session that holds the label now, by
/// that session's own receipt. After a rebind the old session's
/// receipt counts for nothing: the item is owed, undelivered, to the
/// new binding.
#[tokio::test]
async fn after_a_rebind_the_old_sessions_receipt_proves_nothing() {
    use crate::cli::ledger::Delivery;
    let mut s = Standby::under_the_masters_block();
    s.message();
    let delivery = |f: &crate::cli::recovery::Facts| f.ledger.owed()[0].entry.delivery;
    let before = s.facts().await;
    assert_eq!(delivery(&before), Delivery::NotDelivered);

    let identity = before.ledger.owed()[0].entry.identity.clone();
    let recorded = crate::cli::presentation::receive(
        &crate::agent_store::hold_state(s.repo.path(), &s.artist).unwrap(),
        clank_core::Tool::Codex,
        "session-one",
        &[identity],
        50,
        &crate::upgrade::process().settling(),
    );
    assert!(recorded.unwrap(), "the bound session's receipt is taken");
    assert_eq!(
        delivery(&s.facts().await),
        Delivery::Delivered,
        "a receipt from the bound session"
    );

    crate::agent_store::bind_session_to_agent(
        s.repo.path(),
        &s.artist,
        clank_core::Tool::Codex,
        &clank_core::ids::SessionId::parse("session-two").unwrap(),
    )
    .unwrap();
    let rebound = s.facts().await;
    assert_eq!(rebound.session.id.as_str(), "session-two");
    assert_eq!(
        delivery(&rebound),
        Delivery::NotDelivered,
        "the new session was never shown it"
    );
    assert_eq!(rebound.ledger.owed().len(), 1, "and it is still owed");
}

/// A block answered while no monitor ran, and a monitor that starts
/// forty minutes later. The master's promotion stays withheld behind
/// the answer, which is owed from the moment its own file dates, so
/// the forty minutes are already spent and the nudge comes at
/// forty-five, not a whole window after the restart.
#[tokio::test]
async fn an_answer_written_while_no_monitor_ran_is_owed_from_its_file() {
    use crate::cli::ledger::Since;
    crate::cli::recovery::tests::enable(clank_core::Tool::Codex, Some(Transport::Typed));
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
    std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"lead":{"tool":"codex","role":"master"},"artist":{"tool":"codex","role":"standby"}}}"#,
        )
        .unwrap();
    std::fs::write(
        repo.path().join(".clank/queue/050-next-plan.md"),
        "# next\n",
    )
    .unwrap();
    let lead = AgentLabel::parse("lead").unwrap();
    let artist = AgentLabel::parse("artist").unwrap();
    crate::agent_store::update_agent_config(repo.path(), &lead, |c| {
        c.auto_mode = Some(clank_core::AutoMode::On);
    })
    .unwrap();
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &lead,
        clank_core::Tool::Codex,
        &clank_core::ids::SessionId::parse("session-one").unwrap(),
    )
    .unwrap();
    crate::cli::block::ask(repo.path(), &artist, "takes", "Which takes?").unwrap();
    let record = repo.path().join("rollout.jsonl");
    append(&record, &STARTED.replace("\"T\"", "\"t1\""));
    written_at(&record, 1000);

    let facts = || async {
        let state = folded(repo.path()).await.unwrap();
        let mut f = in_turns(
            &mut crate::cli::turn_custody::Turns::new(repo.path().to_path_buf()),
            |custody| {
                custody.facts(
                    repo.path(),
                    &state,
                    &mut Sources::hosting(Default::default()),
                )
            },
        )
        .facts
        .into_iter()
        .find(|f| f.author == lead)
        .unwrap();
        f.session.transcript = Some(record.display().to_string());
        f
    };
    let withheld = facts().await;
    assert!(withheld.ledger.owed().is_empty(), "{withheld:?}");
    assert_eq!(withheld.ledger.entries.len(), 1, "the promotion, withheld");

    let answer = crate::cli::block::answer_block(repo.path(), "takes", "the third", None).unwrap();
    let answered = crate::cli::recovery::unix_now() - 40 * 60;
    written_at(&answer, answered);
    let restart = answered + 40 * 60;

    let released = facts().await;
    let owed = released.ledger.owed();
    let [decision] = owed.as_slice() else {
        panic!("{owed:?}");
    };
    assert_eq!(decision.entry.what, "answer to takes");
    assert_eq!(
        decision.owed_since,
        Since::At(answered),
        "from the answer, not from this fold"
    );

    let stall = released.stall.unwrap();
    assert_eq!((stall.warn, stall.nudge), (40 * 60, Some(5 * 60)));
    let mut d = crate::cli::recovery::Driver::new(
        repo.path().to_path_buf(),
        std::path::PathBuf::from("/nonexistent-home"),
    );
    d.reconcile(vec![released], restart);
    assert_eq!(
        d.next_due(),
        Some(answered + 40 * 60),
        "the window is already spent"
    );
    assert_eq!(dispatched(&d.due(restart, |_, _| Gate::Go)), 0, "warned");
    assert_eq!(
        dispatched(&d.due(answered + 45 * 60 - 1, |_, _| Gate::Go)),
        0
    );
    assert_eq!(
        dispatched(&d.due(answered + 45 * 60, |_, _| Gate::Go)),
        1,
        "nudged at forty-five minutes of being owed"
    );
}

#[test]
fn a_helpers_hello_round_trips() {
    let who = Who::RecoveryHelper {
        author: "opencode".into(),
        holder: Holder {
            tool: clank_core::Tool::OpenCode,
            session: "session-one".into(),
        },
    };
    let wire = serde_json::to_string(&who).unwrap();
    assert!(wire.contains(r#""as":"recovery_helper""#), "{wire}");
    assert_eq!(serde_json::from_str::<Who>(&wire).unwrap(), who);
}
