use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::monitor::{Conn, OUTAGE_BUDGET};
use super::super::wake_render::monitor_outage;

pub(crate) const SESSION: &str = "session-unmonitored";

type Attaching<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Attempt> + Send + 'a>>;

/// A repo whose master `claude` runs `tool`, bound to [`SESSION`], auto
/// on, owed one timer event when `owed`.
pub(crate) fn repo_with(tool: clank_core::Tool, owed: bool) -> (tempfile::TempDir, AgentLabel) {
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
            tool,
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
        tool,
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

fn stop_of(tool: clank_core::Tool, deadline: u64) -> Report {
    Report::TurnEnd {
        stop: clank_core::turn_policy::TurnEndReport {
            tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
            tool,
            session: clank_core::SessionId::parse(SESSION).unwrap(),
            place: Default::default(),
        },
        deadline,
        questions: Vec::new(),
    }
}

fn stop(deadline: u64) -> Report {
    stop_of(clank_core::Tool::Claude, deadline)
}

pub(crate) fn handoff() -> Report {
    handoff_by(u64::MAX)
}

pub(crate) fn handoff_by(deadline: u64) -> Report {
    Report::Handoff {
        tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
        session: clank_core::SessionId::parse(SESSION).unwrap(),
        continued: false,
        deadline,
    }
}

/// A deadline `ahead` of now, as a report names one.
pub(crate) fn within(ahead: std::time::Duration) -> u64 {
    super::super::turn_custody::now_ms() + ahead.as_millis() as u64
}

pub(crate) fn told(repo: &Path, author: &AgentLabel) -> bool {
    crate::agent_store::load_agent_config(repo, author)
        .unwrap()
        .is_some_and(|cfg| cfg.outage_notified.is_some())
}

fn outage_notice() -> Frame {
    Frame::text(
        WireVerdict::Notify,
        monitor_outage("starting one failed: no binary"),
    )
}

fn frames(out: Vec<u8>) -> Vec<Frame> {
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Carry `report` for a hook that stays, through `attach`, and give what
/// was printed. No timeout of its own: under a paused clock one would
/// fire while the owner's thread was still reading.
async fn carried(
    repo: &Path,
    author: &AgentLabel,
    report: &Report,
    attach: Attach<'_>,
) -> Vec<Frame> {
    carried_by(
        repo,
        author,
        report,
        attach,
        &crate::upgrade::Authority::unwatched(),
    )
    .await
}

/// [`carried`], by a child of the build `authority` judges.
async fn carried_by(
    repo: &Path,
    author: &AgentLabel,
    report: &Report,
    attach: Attach<'_>,
    authority: &crate::upgrade::Authority,
) -> Vec<Frame> {
    let (owner_read, mut stdin) = std::io::pipe().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(report).unwrap()).unwrap();
    let mut out = Vec::new();
    carry(
        repo,
        author,
        None,
        Owner::reading(owner_read),
        &mut out,
        attach,
        authority,
    )
    .await;
    drop(stdin);
    frames(out)
}

/// What a child whose every attempt ends as `attempt` prints for `report`,
/// on a repo that owes its agent work. Under a paused clock.
pub(crate) async fn unserved(report: &Report, attempt: fn() -> Attempt) -> Vec<Frame> {
    let (repo, author) = repo_with(report.tool(), true);
    let attach = move |_: Who| -> Attaching<'_> { Box::pin(async move { attempt() }) };
    carried(repo.path(), &author, report, &attach).await
}

pub(crate) fn no_binary() -> Attempt {
    Attempt::SpawnFailed("starting one failed: no binary".into())
}

pub(crate) fn newer() -> Attempt {
    Attempt::Newer("the monitor speaks a newer protocol".into())
}

/// The frame the child prints when no monitor could be had for the whole
/// outage budget, for `tool`'s Stop.
pub(crate) async fn outage_frame(tool: clank_core::Tool) -> Frame {
    let [frame] = unserved(&stop_of(tool, u64::MAX), no_binary)
        .await
        .try_into()
        .expect("one frame");
    frame
}

/// The frame the child prints for a report it cannot parse.
pub(crate) async fn unparsed_frame() -> Frame {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let (owner_read, mut stdin) = std::io::pipe().unwrap();
    writeln!(stdin, "not a report").unwrap();
    let attach = |_: Who| -> Attaching<'_> { panic!("nothing to carry") };
    let mut out = Vec::new();
    carry(
        repo.path(),
        &author,
        None,
        Owner::reading(owner_read),
        &mut out,
        &attach,
        &crate::upgrade::Authority::unwatched(),
    )
    .await;
    let [frame] = frames(out).try_into().expect("one frame");
    frame
}

/// A connection whose monitor has said `said`, and the monitor's end of
/// it: dropping that is the monitor going away.
fn monitor_saying(said: Frame) -> (Conn, std::os::unix::net::UnixStream) {
    let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    writeln!(
        &theirs,
        "{}",
        serde_json::to_string(&Note::Verdict(said)).unwrap()
    )
    .unwrap();
    (Conn::over(mine), theirs)
}

/// A monitor lost three times mid-park is followed three times, and the
/// only verdicts the hook hears are the ones monitors gave: the child
/// says nothing of its own about the work this repo owes.
#[tokio::test]
async fn a_monitor_lost_three_times_mid_park_is_followed_three_times() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let attempts = AtomicUsize::new(0);
    let woke = || Frame::text(WireVerdict::Notify, "the fourth monitor's word".into());
    let staying = std::sync::Mutex::new(None);
    let attach = |_: Who| -> Attaching<'_> {
        let nth = attempts.fetch_add(1, Ordering::SeqCst);
        let conn = match nth {
            0..3 => monitor_saying(Frame::bare(WireVerdict::Park)).0,
            _ => {
                let (conn, monitor) = monitor_saying(woke());
                *staying.lock().unwrap() = Some(monitor);
                conn
            }
        };
        Box::pin(async move { Attempt::Shared(conn) })
    };
    let began = std::time::Instant::now();
    let printed = carried(repo.path(), &author, &stop(u64::MAX), &attach).await;
    let park = Frame::bare(WireVerdict::Park);
    assert_eq!(printed, vec![park.clone(), park.clone(), park, woke()]);
    assert_eq!(attempts.into_inner(), 4, "each lost monitor was followed");
    // Backing off after each loss would sleep 0.5s, 1s and 2s: 3.5s in
    // all. Three seconds tells that apart and leaves a loaded machine room.
    assert!(
        began.elapsed() < std::time::Duration::from_secs(3),
        "a monitor that had answered is followed without a pause: {:?}",
        began.elapsed()
    );
}

/// With no monitor to be had for the whole outage budget, the agent is
/// woken with the reason. It is not parked, not handed a diagnostic, and
/// not presented the work the repo owes it.
#[tokio::test(start_paused = true)]
async fn an_outage_past_its_budget_wakes_with_the_reason_and_no_work() {
    let began = tokio::time::Instant::now();
    let printed = unserved(&stop(u64::MAX), no_binary).await;
    assert_eq!(
        printed,
        vec![Frame::text(
            WireVerdict::Notify,
            monitor_outage("starting one failed: no binary")
        )]
    );
    assert!(printed[0].presentation().is_none());
    assert!(
        !printed[0]
            .rendered
            .as_deref()
            .unwrap()
            .contains("scheduled work")
    );
    let took = began.elapsed();
    assert!(
        took >= OUTAGE_BUDGET && took < OUTAGE_BUDGET * 2,
        "it held on for the budget and no longer: {took:?}"
    );
}

/// A child of a build that has been replaced, refused by the installed
/// build's monitor for three outage budgets: it goes on trying, tells its
/// agent nothing and records no outage, and its park ends as an
/// uneventful one does. Its hook is what moves; the upgrade costs no turn.
#[tokio::test(start_paused = true)]
async fn a_stale_child_goes_on_trying_and_tells_its_agent_nothing() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let installed = crate::upgrade::tests::Installed::current();
    let authority = installed.authority();
    installed.replace(1);
    let attempts = AtomicUsize::new(0);
    let attach = |_: Who| -> Attaching<'_> {
        attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { newer() })
    };
    let report = stop(within(OUTAGE_BUDGET * 3));
    let printed = carried_by(repo.path(), &author, &report, &attach, &authority).await;
    assert_eq!(
        printed,
        vec![super::super::turn_custody::Turns::deadline_frame(
            clank_core::Tool::Claude
        )]
    );
    assert!(!told(repo.path(), &author), "no outage was announced");
    assert!(
        attempts.into_inner() as u32 > OUTAGE_BUDGET.as_secs() as u32 * 2 / 8,
        "it went on trying past the outage budget"
    );
}

/// The same refusal, to a child whose own build is still the installed
/// one, is an outage like any other: nothing says an upgrade explains it.
#[tokio::test(start_paused = true)]
async fn a_newer_monitor_with_no_newer_install_is_an_outage() {
    let printed = unserved(&stop(u64::MAX), newer).await;
    assert_eq!(
        printed,
        vec![Frame::text(
            WireVerdict::Notify,
            monitor_outage("the monitor speaks a newer protocol")
        )]
    );
}

/// A report that never parks is told of a retry still failing at its
/// deadline, inside the outage budget and ahead of its hook's giving up:
/// it has no later chance.
#[tokio::test(start_paused = true)]
async fn a_report_that_never_parks_is_told_at_its_deadline_of_a_retry_still_failing() {
    let left = std::time::Duration::from_secs(10);
    let began = tokio::time::Instant::now();
    let printed = unserved(&handoff_by(within(left)), no_binary).await;
    assert_eq!(printed, vec![outage_notice()]);
    let early = left - began.elapsed();
    let second = std::time::Duration::from_secs(1);
    assert!(
        early >= second && early < DEADLINE_MARGIN + second,
        "it ended ahead of the deadline, not at it and not at the budget: {early:?}"
    );
}

fn re_arm() -> Frame {
    Frame::text(
        WireVerdict::Notify,
        super::super::wake_render::REARM_REASON.to_string(),
    )
}

/// A park's deadline is not an outage. A retry still failing when it
/// comes ends as an uneventful park does, and the session is told
/// nothing: its hook arms the next park, whose child has a budget of its
/// own.
#[tokio::test(start_paused = true)]
async fn a_parks_deadline_inside_an_outage_re_arms_and_announces_nothing() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let attach = |_: Who| -> Attaching<'_> { Box::pin(async { no_binary() }) };
    let ten = std::time::Duration::from_secs(10);
    let printed = carried(repo.path(), &author, &stop(within(ten)), &attach).await;
    assert_eq!(printed, vec![re_arm()]);
    assert!(!told(repo.path(), &author));
}

/// A parked report whose monitor goes after the child's last word, with
/// the deadline all but here, re-arms: nothing is announced, no marker is
/// claimed, and no second monitor is asked for.
///
/// On the real clock: the report and the monitor's going both reach the
/// child through threads, which a paused clock would race past.
#[tokio::test]
async fn a_park_that_loses_its_monitor_after_its_last_word_re_arms_and_announces_nothing() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let attempts = AtomicUsize::new(0);
    let monitor = std::sync::Mutex::new(None);
    let attach = |_: Who| -> Attaching<'_> {
        attempts.fetch_add(1, Ordering::SeqCst);
        let (conn, theirs) = monitor_saying(Frame::bare(WireVerdict::Park));
        *monitor.lock().unwrap() = Some(theirs);
        Box::pin(async move { Attempt::Shared(conn) })
    };
    let left = DEADLINE_MARGIN + std::time::Duration::from_secs(1);
    let park = stop(within(left));
    let leaves = async {
        tokio::time::sleep(left - DEADLINE_MARGIN / 2).await;
        drop(monitor.lock().unwrap().take().expect("the child attached"));
    };
    let (printed, ()) = tokio::join!(carried(repo.path(), &author, &park, &attach), leaves);
    assert_eq!(printed, vec![Frame::bare(WireVerdict::Park), re_arm()]);
    assert_eq!(attempts.into_inner(), 1);
    assert!(!told(repo.path(), &author));
}

/// One outage is announced once. The Stops after it wake nobody about
/// it: each child goes on trying for its whole park, well past the
/// budget, and ends as an uneventful park does.
#[tokio::test(start_paused = true)]
async fn a_second_and_third_stop_in_one_outage_go_on_trying_and_say_nothing_of_it() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let attempts = AtomicUsize::new(0);
    let attach = |_: Who| -> Attaching<'_> {
        attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { no_binary() })
    };
    let first = carried(repo.path(), &author, &stop(u64::MAX), &attach).await;
    assert_eq!(first, vec![outage_notice()]);
    assert!(told(repo.path(), &author));

    let park = OUTAGE_BUDGET * 5;
    for nth in ["second", "third"] {
        let before = attempts.load(Ordering::SeqCst);
        let began = tokio::time::Instant::now();
        let printed = carried(repo.path(), &author, &stop(within(park)), &attach).await;
        assert_eq!(
            printed,
            vec![Frame::text(
                WireVerdict::Notify,
                super::super::wake_render::REARM_REASON.to_string()
            )],
            "the {nth} Stop ends as a park with nothing to say does"
        );
        let took = began.elapsed();
        assert!(
            took > OUTAGE_BUDGET * 4 && took <= park,
            "the {nth} Stop held its whole park: {took:?}"
        );
        assert!(
            attempts.load(Ordering::SeqCst) - before > 30,
            "and went on attempting throughout"
        );
        assert!(told(repo.path(), &author));
    }
}

/// A monitor's answer ends the outage, so the next one is announced
/// again.
#[tokio::test(start_paused = true)]
async fn an_outage_after_a_monitor_answered_is_announced_again() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let back = std::sync::atomic::AtomicBool::new(false);
    let running = std::sync::atomic::AtomicBool::new(false);
    // The monitor's word reaches the child through a thread of its own,
    // which a paused clock would race past: time runs while it is heard.
    let attach = |_: Who| -> Attaching<'_> {
        let attempt = match back.swap(false, Ordering::SeqCst) {
            true => {
                tokio::time::resume();
                running.store(true, Ordering::SeqCst);
                Attempt::Shared(monitor_saying(Frame::bare(WireVerdict::Park)).0)
            }
            false => {
                if running.swap(false, Ordering::SeqCst) {
                    tokio::time::pause();
                }
                no_binary()
            }
        };
        Box::pin(async move { attempt })
    };
    let first = carried(repo.path(), &author, &stop(u64::MAX), &attach).await;
    assert_eq!(first, vec![outage_notice()]);
    assert!(told(repo.path(), &author));

    back.store(true, Ordering::SeqCst);
    let park = stop(within(OUTAGE_BUDGET * 5));
    let second = carried(repo.path(), &author, &park, &attach).await;
    assert_eq!(
        second[0],
        Frame::bare(WireVerdict::Park),
        "the monitor that came back answered"
    );
    assert_eq!(
        second[1..],
        [outage_notice()],
        "and its loss is a new outage, announced after its own budget"
    );
    assert!(told(repo.path(), &author));
}

/// A handoff in an outage its session was already told of is not answered
/// by any rule of the child's own: Grok's Stop gets a diagnostic, which
/// blocks nothing.
#[tokio::test(start_paused = true)]
async fn a_handoff_in_an_outage_already_announced_blocks_nothing() {
    let (repo, author) = repo_with(clank_core::Tool::Grok, false);
    let attach = |_: Who| -> Attaching<'_> { Box::pin(async { no_binary() }) };
    let first = carried(repo.path(), &author, &handoff(), &attach).await;
    assert_eq!(first, vec![outage_notice()]);
    let ten = std::time::Duration::from_secs(10);
    let second = carried(repo.path(), &author, &handoff_by(within(ten)), &attach).await;
    assert!(
        matches!(
            second.as_slice(),
            [Frame { verdict: WireVerdict::Diagnostic { message }, .. }]
                if message.contains("still has no monitor")
        ),
        "{second:?}"
    );
}

/// The hook going away ends a failing retry at once, with nothing said.
#[tokio::test]
async fn the_owners_eof_ends_a_failing_retry_at_once() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let (owner_read, mut stdin) = std::io::pipe().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(&stop(u64::MAX)).unwrap()).unwrap();
    drop(stdin);
    let attach = |_: Who| -> Attaching<'_> { Box::pin(async { no_binary() }) };
    let mut out = Vec::new();
    let began = std::time::Instant::now();
    carry(
        repo.path(),
        &author,
        None,
        Owner::reading(owner_read),
        &mut out,
        &attach,
        &crate::upgrade::Authority::unwatched(),
    )
    .await;
    assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
    assert!(
        began.elapsed() < OUTAGE_BUDGET / 4,
        "it did not wait out the budget: {:?}",
        began.elapsed()
    );
}

/// A session that no longer holds the label is owed nothing, not even the
/// notice: the child stops trying and says nothing.
#[tokio::test(start_paused = true)]
async fn a_superseded_binding_ends_a_failing_retry_with_nothing_said() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &author,
        clank_core::Tool::Claude,
        &clank_core::SessionId::parse("session-that-took-over").unwrap(),
    )
    .unwrap();
    let attempts = AtomicUsize::new(0);
    let attach = |_: Who| -> Attaching<'_> {
        attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { no_binary() })
    };
    let printed = carried(repo.path(), &author, &stop(u64::MAX), &attach).await;
    assert_eq!(printed, Vec::new());
    assert_eq!(attempts.into_inner(), 1);
}

/// The notice wakes only a session clank drives: with auto off the outage
/// ends in silence.
#[tokio::test(start_paused = true)]
async fn an_outage_says_nothing_to_an_agent_with_auto_off() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    crate::agent_store::set_auto_mode(repo.path(), &author, clank_core::AutoMode::Off).unwrap();
    let attach = |_: Who| -> Attaching<'_> { Box::pin(async { no_binary() }) };
    let printed = carried(repo.path(), &author, &stop(u64::MAX), &attach).await;
    assert_eq!(
        printed,
        vec![Frame::bare(WireVerdict::Silent {
            why: clank_core::SilentReason::AutoOff
        })]
    );
}

/// A report that does not parse is still the diagnostic it was, which
/// wakes nobody.
#[tokio::test]
async fn an_unparsed_report_is_still_a_diagnostic() {
    let frame = unparsed_frame().await;
    assert!(
        matches!(&frame.verdict, WireVerdict::Diagnostic { message } if message.contains("did not parse")),
        "{frame:?}"
    );
}

/// A hook gone before it said anything ends the child with nothing said.
#[tokio::test]
async fn a_hook_gone_before_its_report_ends_the_child() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let (owner_read, stdin) = std::io::pipe().unwrap();
    drop(stdin);
    let attach = |_: Who| -> Attaching<'_> { panic!("nothing to carry") };
    let mut out = Vec::new();
    carry(
        repo.path(),
        &author,
        None,
        Owner::reading(owner_read),
        &mut out,
        &attach,
        &crate::upgrade::Authority::unwatched(),
    )
    .await;
    assert!(out.is_empty());
}

/// Grok's handoff is the monitor's to answer like any other report, and
/// it is carried without holding the session, so it can never take the
/// place of the very wait it reports.
#[tokio::test(start_paused = true)]
async fn a_handoff_is_carried_to_a_monitor_and_holds_no_wait() {
    let (repo, author) = repo_with(clank_core::Tool::Grok, false);
    let held = std::sync::Mutex::new(Vec::new());
    let attach = |who: Who| -> Attaching<'_> {
        if let Who::Agent { holder, .. } = who {
            held.lock().unwrap().push(holder);
        }
        Box::pin(async { newer() })
    };
    let (owner_read, mut stdin) = std::io::pipe().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(&handoff()).unwrap()).unwrap();
    let mut out = Vec::new();
    carry(
        repo.path(),
        &author,
        Some(SESSION.into()),
        Owner::reading(owner_read),
        &mut out,
        &attach,
        &crate::upgrade::Authority::unwatched(),
    )
    .await;
    drop(stdin);
    let held = held.lock().unwrap();
    assert!(
        !held.is_empty() && held.iter().all(Option::is_none),
        "{held:?}"
    );
    assert_eq!(
        frames(out),
        vec![Frame::text(
            WireVerdict::Notify,
            monitor_outage("the monitor speaks a newer protocol")
        )]
    );
}

/// With no monitor, a handoff gets the same notice as any report: the
/// child answers nothing by the handoff rule itself.
#[tokio::test(start_paused = true)]
async fn a_handoff_with_no_monitor_is_told_so_and_not_asked_to_arm_a_wait() {
    let printed = unserved(&handoff(), no_binary).await;
    assert_eq!(
        printed,
        vec![Frame::text(
            WireVerdict::Notify,
            monitor_outage("starting one failed: no binary")
        )]
    );
}

/// A connection whose monitor beats and never gives a verdict, with the
/// monitor's end of it.
fn monitor_only_beating() -> (Conn, std::os::unix::net::UnixStream) {
    let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    writeln!(&theirs, "{}", serde_json::to_string(&Note::Beat).unwrap()).unwrap();
    (Conn::over(mine), theirs)
}

/// A handoff a monitor took and did not answer is told so before its hook
/// gives up, like one no monitor took: being connected does not put the
/// exchange outside the report's deadline. It is announced once; the
/// next gets a diagnostic, which blocks nothing.
#[tokio::test(start_paused = true)]
async fn a_handoff_a_monitor_took_and_never_answered_is_told_so_before_its_deadline() {
    let (repo, author) = repo_with(clank_core::Tool::Grok, true);
    let monitors = std::sync::Mutex::new(Vec::new());
    let attach = |_: Who| -> Attaching<'_> {
        let (conn, monitor) = monitor_only_beating();
        monitors.lock().unwrap().push(monitor);
        Box::pin(async move { Attempt::Shared(conn) })
    };
    let ten = std::time::Duration::from_secs(10);
    let began = tokio::time::Instant::now();
    let first = carried(repo.path(), &author, &handoff_by(within(ten)), &attach).await;
    assert_eq!(
        first,
        vec![Frame::text(
            WireVerdict::Notify,
            super::super::wake_render::MONITOR_UNANSWERED.to_string()
        )]
    );
    let spare = ten.saturating_sub(began.elapsed());
    assert!(
        spare >= std::time::Duration::from_secs(1),
        "the hook had {spare:?} of its wait left to read it in"
    );
    assert!(told(repo.path(), &author), "a beat is not an answer");

    let second = carried(repo.path(), &author, &handoff_by(within(ten)), &attach).await;
    assert!(
        matches!(
            second.as_slice(),
            [Frame { verdict: WireVerdict::Diagnostic { message }, .. }]
                if message.contains("has not answered")
        ),
        "{second:?}"
    );
}

/// A parked report whose monitor is there and says nothing at its
/// deadline ends as an uneventful park does: it is not an outage, and
/// the monitor had the deadline itself to answer in first.
#[tokio::test(start_paused = true)]
async fn a_connected_park_unanswered_past_its_deadline_re_arms_and_is_no_outage() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let monitors = std::sync::Mutex::new(Vec::new());
    let attach = |_: Who| -> Attaching<'_> {
        let (conn, monitor) = monitor_only_beating();
        monitors.lock().unwrap().push(monitor);
        Box::pin(async move { Attempt::Shared(conn) })
    };
    let ten = std::time::Duration::from_secs(10);
    let began = tokio::time::Instant::now();
    let printed = carried(repo.path(), &author, &stop(within(ten)), &attach).await;
    assert_eq!(
        printed,
        vec![Frame::text(
            WireVerdict::Notify,
            super::super::wake_render::REARM_REASON.to_string()
        )]
    );
    let took = began.elapsed();
    assert!(
        took >= ten && took < ten + DEADLINE_MARGIN * 2,
        "after the monitor's own chance at the deadline: {took:?}"
    );
    assert!(!told(repo.path(), &author));
}

/// The hook going away ends a connected exchange at once, with nothing
/// said, whatever the deadline.
#[tokio::test]
async fn the_owners_eof_ends_a_connected_exchange_at_once() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let (owner_read, mut stdin) = std::io::pipe().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(&stop(u64::MAX)).unwrap()).unwrap();
    drop(stdin);
    let monitors = std::sync::Mutex::new(Vec::new());
    let attach = |_: Who| -> Attaching<'_> {
        let (conn, monitor) = monitor_only_beating();
        monitors.lock().unwrap().push(monitor);
        Box::pin(async move { Attempt::Shared(conn) })
    };
    let mut out = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        carry(
            repo.path(),
            &author,
            None,
            Owner::reading(owner_read),
            &mut out,
            &attach,
            &crate::upgrade::Authority::unwatched(),
        ),
    )
    .await
    .expect("the child ends with its hook");
    assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
}

/// A hook's pipe that takes `hold` to accept each write and says on
/// `writing` when one began, or refuses every write when `broken`.
pub(crate) struct HookPipe {
    pub(crate) bytes: Vec<u8>,
    pub(crate) hold: std::time::Duration,
    pub(crate) writing: Option<std::sync::mpsc::Sender<()>>,
    pub(crate) broken: bool,
}

impl Write for HookPipe {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.broken {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        if let Some(writing) = self.writing.take() {
            let _ = writing.send(());
        }
        std::thread::sleep(self.hold);
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One Grok Stop's child with no monitor to be had, run to its end on
/// this thread with a clock of its own, printing into `out`. Its handoff
/// leaves it ten seconds, so it speaks at its last word.
pub(crate) fn handoff_child(repo: &Path, author: &AgentLabel, out: &mut HookPipe) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
        .block_on(async {
            let (owner_read, mut stdin) = std::io::pipe().unwrap();
            let report = handoff_by(within(std::time::Duration::from_secs(10)));
            writeln!(stdin, "{}", serde_json::to_string(&report).unwrap()).unwrap();
            let attach = |_: Who| -> Attaching<'_> { Box::pin(async { no_binary() }) };
            carry(
                repo,
                author,
                None,
                Owner::reading(owner_read),
                out,
                &attach,
                &crate::upgrade::Authority::unwatched(),
            )
            .await;
            drop(stdin);
        });
}

pub(crate) fn announcements(out: &HookPipe) -> usize {
    frames(out.bytes.clone())
        .iter()
        .filter(|frame| frame.verdict == WireVerdict::Notify)
        .count()
}

/// Two hooks of one session whose children reach the notice together
/// wake it once: the second starts while the first is still writing, and
/// finds the session told.
#[test]
fn two_children_reaching_the_notice_together_announce_one_outage_once() {
    let (repo, author) = repo_with(clank_core::Tool::Grok, false);
    let (writing, began) = std::sync::mpsc::channel();
    let mut first = HookPipe {
        bytes: Vec::new(),
        hold: std::time::Duration::from_secs(1),
        writing: Some(writing),
        broken: false,
    };
    let mut second = HookPipe {
        bytes: Vec::new(),
        hold: std::time::Duration::ZERO,
        writing: None,
        broken: false,
    };
    std::thread::scope(|scope| {
        scope.spawn(|| handoff_child(repo.path(), &author, &mut first));
        began
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the first child reaches its notice");
        handoff_child(repo.path(), &author, &mut second);
    });
    assert_eq!(announcements(&first), 1);
    assert_eq!(
        announcements(&second),
        0,
        "{:?}",
        String::from_utf8_lossy(&second.bytes)
    );
    assert!(told(repo.path(), &author));
}

/// A notice that never left for a hook already gone tells nobody, so the
/// session is not recorded as told and its next Stop is.
#[test]
fn a_notice_its_hook_never_took_leaves_the_session_untold() {
    let (repo, author) = repo_with(clank_core::Tool::Grok, false);
    let mut gone = HookPipe {
        bytes: Vec::new(),
        hold: std::time::Duration::ZERO,
        writing: None,
        broken: true,
    };
    handoff_child(repo.path(), &author, &mut gone);
    assert!(!told(repo.path(), &author));

    let mut next = HookPipe {
        broken: false,
        ..gone
    };
    handoff_child(repo.path(), &author, &mut next);
    assert_eq!(announcements(&next), 1);
    assert!(told(repo.path(), &author));
}

/// Nothing the child is made of can fold the repo or watch it.
#[test]
fn the_child_names_no_fold_and_no_watcher() {
    let src = include_str!("turn_child.rs");
    for local in [
        "wait::items",
        "rebuild",
        "repo_watch",
        "owed_for",
        "items_for",
        "handoff_verdict",
    ] {
        assert!(
            !src.contains(local),
            "an agent's wake may not come from `{local}`"
        );
    }
}

fn presentation() -> clank_core::turn_policy::PresentationId {
    clank_core::turn_policy::PresentationId {
        session: clank_core::SessionId::parse("session-one").unwrap(),
        seq: 4,
    }
}

/// A hook that takes a presentation and then says nothing, with its pipe
/// still open, is not waited on for ever: the child answers for it as a
/// failed emit just inside the monitor's own bound, so the monitor hears
/// it from the carrier before it gives the presentation up by itself.
#[tokio::test(start_paused = true)]
async fn a_hook_that_never_answers_for_its_presentation_is_a_failed_emit() {
    let (owner_read, stdin) = std::io::pipe().unwrap();
    let mut owner = Owner::reading(owner_read);
    let id = presentation();
    let started = tokio::time::Instant::now();
    let result = super::result_of(&id, &mut owner).await;
    assert_eq!(started.elapsed(), super::RESULT_WITHIN);
    assert!(
        super::RESULT_WITHIN
            < std::time::Duration::from_millis(clank_core::turn_policy::DELIVERY_RESULT_WITHIN),
        "the carrier speaks first"
    );
    let TurnResult::EmitFailed { presentation, why } = result else {
        panic!("{result:?}");
    };
    assert_eq!(presentation, id);
    assert_eq!(why, "the hook gave no result in time");
    drop(stdin);
}

/// A result the hook does give is forwarded as given, and a pipe that
/// closes first is a failed emit, at once.
#[tokio::test]
async fn a_hooks_own_result_is_what_is_forwarded() {
    use std::io::Write as _;
    let id = presentation();
    let (owner_read, mut stdin) = std::io::pipe().unwrap();
    let mut owner = Owner::reading(owner_read);
    let delivered = TurnResult::Delivered {
        presentation: id.clone(),
    };
    writeln!(stdin, "{}", serde_json::to_string(&delivered).unwrap()).unwrap();
    assert_eq!(super::result_of(&id, &mut owner).await, delivered);

    let (owner_read, stdin) = std::io::pipe().unwrap();
    let mut owner = Owner::reading(owner_read);
    drop(stdin);
    let started = tokio::time::Instant::now();
    assert!(matches!(
        super::result_of(&id, &mut owner).await,
        TurnResult::EmitFailed { .. }
    ));
    assert!(started.elapsed() < super::RESULT_WITHIN);
}

fn retiring() -> Attempt {
    Attempt::LeaseHeld("the retiring monitor still holds the lease".into())
}

/// An upgrade as a freshly started child of the installed build meets it:
/// the old monitor holds the lease for longer than an outage is allowed,
/// while it hears out its hooks and drains, and then the installed
/// monitor answers. The park is held through all of it, the agent is told
/// nothing, and the only verdict printed is the new monitor's.
#[tokio::test(start_paused = true)]
async fn a_park_outwaits_a_retiring_monitor_longer_than_an_outage() {
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    let seventy = OUTAGE_BUDGET + std::time::Duration::from_secs(10);
    let began = tokio::time::Instant::now();
    let woke = || Frame::text(WireVerdict::Notify, "the installed monitor's word".into());
    let staying = std::sync::Mutex::new(None);
    let attach = |_: Who| -> Attaching<'_> {
        let attempt = match began.elapsed() < seventy {
            true => retiring(),
            false => {
                let (conn, monitor) = monitor_saying(woke());
                *staying.lock().unwrap() = Some(monitor);
                Attempt::Shared(conn)
            }
        };
        Box::pin(async move { attempt })
    };
    let printed = carried(repo.path(), &author, &stop(u64::MAX), &attach).await;
    assert_eq!(printed, vec![woke()]);
    assert!(began.elapsed() >= seventy);
    assert!(!told(repo.path(), &author), "a retirement is not an outage");
}

/// A monitor that is going and never goes is an outage after all, once
/// everything it and its successor are allowed has passed, and no sooner.
#[tokio::test(start_paused = true)]
async fn a_retirement_that_never_ends_is_told_at_its_own_bound() {
    use super::super::monitor::RETIREMENT_BUDGET;
    let began = tokio::time::Instant::now();
    let printed = unserved(&stop(u64::MAX), retiring).await;
    assert_eq!(
        printed,
        vec![Frame::text(
            WireVerdict::Notify,
            monitor_outage("the retiring monitor still holds the lease")
        )]
    );
    let took = began.elapsed();
    assert!(
        took >= RETIREMENT_BUDGET && took < RETIREMENT_BUDGET + OUTAGE_BUDGET,
        "{took:?}"
    );
}

/// A report that cannot park, caught by a retirement, costs its agent no
/// turn either way it meets it: unattached behind the old monitor's
/// lease until its deadline, or attached to an old monitor that answers
/// nothing new while it settles. Each ends with a diagnostic, which wakes
/// nobody, and no notice is recorded as given.
#[tokio::test(start_paused = true)]
async fn a_report_that_cannot_wait_out_a_retirement_ends_quietly() {
    use super::super::monitor::{DescribedBuild, Descriptor};
    let quiet = |printed: &[Frame]| {
        matches!(
            printed,
            [Frame {
                verdict: WireVerdict::Diagnostic { .. },
                rendered: None
            }]
        )
    };
    let ten = std::time::Duration::from_secs(10);

    let printed = unserved(&handoff_by(within(ten)), retiring).await;
    assert!(quiet(&printed), "{printed:?}");

    let (repo, author) = repo_with(clank_core::Tool::Grok, true);
    let mut other = crate::build_stamp::running().0;
    other[0] ^= 1;
    super::super::monitor::publish(
        repo.path(),
        &Descriptor {
            build: Some(DescribedBuild {
                running: crate::build_stamp::Stamp(other).to_string(),
                version: None,
                installed_at: "/bin/clank".into(),
                source: crate::build_stamp::Source::Invocation,
            }),
            ..super::super::monitor::describe(
                repo.path(),
                std::process::id() as i32,
                Path::new("/nowhere.sock"),
            )
        },
    )
    .unwrap();
    let monitors = std::sync::Mutex::new(Vec::new());
    let attach = |_: Who| -> Attaching<'_> {
        let (conn, monitor) = monitor_only_beating();
        monitors.lock().unwrap().push(monitor);
        Box::pin(async move { Attempt::Shared(conn) })
    };
    let printed = carried(repo.path(), &author, &handoff_by(within(ten)), &attach).await;
    let _ = std::fs::remove_file(super::super::monitor::runtime_path(repo.path(), "json"));
    assert!(quiet(&printed), "{printed:?}");
    assert!(!told(repo.path(), &author));
}

/// A park whose child is not the client repairing the repo, while the
/// old monitor is still going: it is told only that another client is
/// starting one, for longer than an outage is allowed, and holds on
/// without a word to its agent until the installed monitor answers.
#[tokio::test(start_paused = true)]
async fn a_park_behind_another_clients_repair_outwaits_the_retirement() {
    use super::super::monitor::{Descriptor, PROTO};
    let (repo, author) = repo_with(clank_core::Tool::Claude, true);
    super::super::monitor::publish(
        repo.path(),
        &Descriptor {
            proto: PROTO - 1,
            ..super::super::monitor::describe(
                repo.path(),
                std::process::id() as i32,
                Path::new("/nowhere.sock"),
            )
        },
    )
    .unwrap();
    let seventy = OUTAGE_BUDGET + std::time::Duration::from_secs(10);
    let began = tokio::time::Instant::now();
    let woke = || Frame::text(WireVerdict::Notify, "the installed monitor's word".into());
    let staying = std::sync::Mutex::new(None);
    let attach = |_: Who| -> Attaching<'_> {
        let attempt = match began.elapsed() < seventy {
            true => Attempt::Busy("another client is already starting one".into()),
            false => {
                let (conn, monitor) = monitor_saying(woke());
                *staying.lock().unwrap() = Some(monitor);
                Attempt::Shared(conn)
            }
        };
        Box::pin(async move { attempt })
    };
    let printed = carried(repo.path(), &author, &stop(u64::MAX), &attach).await;
    let _ = std::fs::remove_file(super::super::monitor::runtime_path(repo.path(), "json"));
    assert_eq!(printed, vec![woke()]);
    assert!(began.elapsed() >= seventy);
    assert!(!told(repo.path(), &author));
}
