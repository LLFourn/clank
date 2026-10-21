use super::*;

const SESSION: &str = "session-alone";

/// A repo whose master `claude` is bound to [`SESSION`], auto on, owed one
/// timer event when `owed`.
fn repo_with(owed: bool) -> (tempfile::TempDir, AgentLabel) {
    use crate::cli::github::log::{EventLog, Transport as Wal};
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    let author = AgentLabel::parse("claude").unwrap();
    crate::cli::agent::add_repo_roster_agent(
        repo.path(),
        &author,
        crate::cli::teams_config::AgentDescription {
            tool: clank_core::Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
        crate::cli::teams_config::RosterRole::Commit,
    )
    .unwrap();
    crate::cli::agent::set_repo_master(repo.path(), &author).unwrap();
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &author,
        clank_core::Tool::Claude,
        &clank_core::SessionId::parse(SESSION).unwrap(),
    )
    .unwrap();
    crate::agent_store::set_auto_mode(repo.path(), &author, clank_core::AutoMode::On).unwrap();
    if owed {
        let timer = clank_core::agent_config::TimerSource {
            interrupt: false,
            interrupt_message: None,
            name: "check".into(),
            repeat_every: None,
            due_at: "1970-01-01T00:01:40Z".into(),
            content: Some("scheduled work".into()),
        };
        crate::agent_store::update_agent_config(repo.path(), &author, |config| {
            config.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
                timer.clone(),
            )];
        })
        .unwrap();
        let events = crate::agent_store::agents_root(repo.path()).join("claude/events");
        std::fs::create_dir_all(&events).unwrap();
        EventLog::open(&events, &timer.source_key(None))
            .unwrap()
            .append_inbox(
                Wal::Timer,
                Some("timer/100"),
                None,
                Some(100),
                &clank_core::wait::WaitItem::TimerEvent {
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
    }
    (repo, author)
}

fn stop(session: &str, deadline: u64) -> Report {
    Report::TurnEnd {
        stop: clank_core::turn_policy::TurnEndReport {
            tool: clank_core::Tool::Claude,
            session: clank_core::SessionId::parse(session).unwrap(),
            place: Default::default(),
            tasks: clank_core::turn_policy::TaskSnapshot::Known(vec![
                serde_json::from_value(serde_json::json!({
                    "id": "a1", "type": "subagent", "status": "running", "description": "sleeper",
                }))
                .unwrap(),
            ]),
        },
        deadline,
        questions: Vec::new(),
    }
}

/// Carry `report` with no monitor to take it, and give what was printed.
async fn alone(repo: &Path, author: &AgentLabel, report: &Report) -> Vec<Frame> {
    use std::io::Write;
    let (owner_read, mut stdin) = std::io::pipe().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(report).unwrap()).unwrap();
    let attach =
        |_: Who| -> std::pin::Pin<Box<dyn std::future::Future<Output = Attached> + Send + '_>> {
            Box::pin(async { Attached::Alone("no monitor in this test".into()) })
        };
    let mut out = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        carry(
            repo,
            author,
            None,
            Owner::reading(owner_read),
            &mut out,
            &attach,
        ),
    )
    .await
    .expect("the child answers");
    drop(stdin);
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// With no monitor, owed work wakes the agent with a notice, never the
/// work: nothing is presented, so nothing can be received for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alone_owed_work_wakes_with_a_notice_and_presents_nothing() {
    let (repo, author) = repo_with(true);
    let frames = alone(repo.path(), &author, &stop(SESSION, u64::MAX)).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].verdict, WireVerdict::Notify);
    assert_eq!(
        frames[0].rendered.as_deref(),
        Some(super::super::wake_render::MONITOR_UNAVAILABLE)
    );
    assert!(frames[0].presentation().is_none());
}

/// Alone, the gates still hold: a stale binding and auto off are quiet,
/// and a live attendance is never honoured — it is no reason to be quiet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alone_keeps_the_gates_and_never_honours_attendance() {
    let (repo, author) = repo_with(true);
    let frames = alone(
        repo.path(),
        &author,
        &stop("session-someone-else", u64::MAX),
    )
    .await;
    assert_eq!(
        frames[0].verdict,
        WireVerdict::Silent {
            why: clank_core::SilentReason::StaleBinding
        }
    );
    crate::agent_store::set_auto_mode(repo.path(), &author, clank_core::AutoMode::Off).unwrap();
    let frames = alone(repo.path(), &author, &stop(SESSION, u64::MAX)).await;
    assert_eq!(
        frames[0].verdict,
        WireVerdict::Silent {
            why: clank_core::SilentReason::AutoOff
        }
    );
}

/// Alone with nothing owed, the wait holds until its deadline and then
/// re-arms; it is never quiet on the subagent the Stop listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alone_with_nothing_owed_holds_until_the_deadline_then_re_arms() {
    let (repo, author) = repo_with(false);
    let deadline = super::super::turn_custody::now_ms() + 1_500;
    let frames = alone(repo.path(), &author, &stop(SESSION, deadline)).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].verdict, WireVerdict::Notify);
    assert_eq!(
        frames[0].rendered.as_deref(),
        Some(super::super::wake_render::REARM_REASON)
    );
}

/// A session start alone surfaces owed work as the notice, and says
/// nothing when nothing is owed: it never parks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_start_alone_says_the_notice_or_nothing() {
    let start = Report::SessionStart {
        tool: clank_core::Tool::Claude,
        session: clank_core::SessionId::parse(SESSION).unwrap(),
        deadline: u64::MAX,
    };
    let (repo, author) = repo_with(true);
    let frames = alone(repo.path(), &author, &start).await;
    assert_eq!(frames[0].verdict, WireVerdict::Notify);
    let (repo, author) = repo_with(false);
    let frames = alone(repo.path(), &author, &start).await;
    assert_eq!(frames[0].verdict, WireVerdict::Park);
}

/// A hook gone before it said anything ends the child with nothing said.
#[tokio::test]
async fn a_hook_gone_before_its_report_ends_the_child() {
    let (repo, author) = repo_with(true);
    let (owner_read, stdin) = std::io::pipe().unwrap();
    drop(stdin);
    let attach =
        |_: Who| -> std::pin::Pin<Box<dyn std::future::Future<Output = Attached> + Send + '_>> {
            panic!("nothing to carry")
        };
    let mut out = Vec::new();
    carry(
        repo.path(),
        &author,
        None,
        Owner::reading(owner_read),
        &mut out,
        &attach,
    )
    .await;
    assert!(out.is_empty());
}

/// Grok's handoff is answered as the monitor answers it, with no monitor
/// and no fold: a request for the wait while none runs, silence while one
/// does. It is carried without holding the session, so it can never take
/// the place of the very wait it reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handoff_alone_is_answered_by_the_native_rule_and_holds_no_wait() {
    let (repo, author) = repo_with(false);
    let session = clank_core::SessionId::parse(SESSION).unwrap();
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &author,
        clank_core::Tool::Grok,
        &session,
    )
    .unwrap();
    let handoff = |wait_running| Report::Handoff {
        handoff: clank_core::turn_policy::Handoff {
            session: session.clone(),
            wait_running,
            continued: false,
        },
        deadline: u64::MAX,
    };
    let frames = alone(repo.path(), &author, &handoff(false)).await;
    assert_eq!(
        frames,
        vec![Frame::text(
            WireVerdict::Notify,
            crate::cli::wake_render::ARM_WAIT.to_string()
        )]
    );
    let frames = alone(repo.path(), &author, &handoff(true)).await;
    assert_eq!(
        frames,
        vec![Frame::bare(WireVerdict::Silent {
            why: clank_core::SilentReason::WaitArmed
        })]
    );

    use std::io::Write;
    let (owner_read, mut stdin) = std::io::pipe().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(&handoff(false)).unwrap()).unwrap();
    let held = std::sync::Mutex::new(Vec::new());
    let attach =
        |who: Who| -> std::pin::Pin<Box<dyn std::future::Future<Output = Attached> + Send + '_>> {
            if let Who::Agent { holder, .. } = who {
                held.lock().unwrap().push(holder);
            }
            Box::pin(async { Attached::Alone("no monitor in this test".into()) })
        };
    carry(
        repo.path(),
        &author,
        Some(SESSION.into()),
        Owner::reading(owner_read),
        &mut Vec::new(),
        &attach,
    )
    .await;
    drop(stdin);
    assert_eq!(*held.lock().unwrap(), vec![None]);
}
