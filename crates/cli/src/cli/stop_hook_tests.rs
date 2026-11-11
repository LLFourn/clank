use super::*;

fn stated() -> crate::agent_env::Stated {
    crate::agent_env::Stated::default()
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "--quiet"])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    dir
}

fn hook_input(session: &str) -> HookInput {
    serde_json::from_value(serde_json::json!({
        "session_id": session,
        "cwd": "/tmp",
        "stop_hook_active": false,
    }))
    .unwrap()
}

/// When the hooks these tests run say they started.
const HOOK_STARTED: u64 = 1_794_355_246_100;

/// `input`, as read from a harness just now.
fn arrived(input: HookInput) -> Arrived {
    Arrived {
        raw: String::new(),
        input,
        resumed: None,
        poll_deadline: poll_deadline(std::time::Instant::now()),
        hook_started: Some(HOOK_STARTED),
    }
}

/// A host that only starts carriers: nothing is installed over it.
struct Spawns<'a>(&'a dyn Spawn);

impl Host for Spawns<'_> {
    fn now_ms(&self) -> Ms {
        HOOK_STARTED
    }
    fn input(&mut self) -> Result<String, String> {
        unreachable!("the input is supplied")
    }
    fn spawner(&self) -> &dyn Spawn {
        self.0
    }
    fn streams(&mut self) -> (&mut dyn std::io::Write, &mut dyn std::io::Write) {
        unreachable!("nothing is emitted through the host")
    }
    fn exit(&mut self, _: i32) {}
    fn authority(&self) -> crate::upgrade::Authority {
        crate::upgrade::Authority::unwatched()
    }
    fn become_installed(&self, _: &Path) -> std::io::Error {
        panic!("nothing is installed over a test")
    }
}

fn bind_tool(repo: &Path, label: &str, session: &str, tool: Tool) -> AgentLabel {
    let label = AgentLabel::parse(label).unwrap();
    let sid = clank_core::ids::SessionId::parse(session).unwrap();
    crate::agent_store::bind_session_to_agent(repo, &label, tool, &sid).unwrap();
    label
}

/// A spawner that records what it was asked to carry, and carries nothing.
#[derive(Default)]
struct Recorder(std::cell::RefCell<Vec<(PathBuf, String, Report)>>);

impl Spawn for Recorder {
    fn spawn(
        &self,
        repo: &Path,
        label: &AgentLabel,
        _session: &str,
        _tool: Tool,
        report: &Report,
    ) -> Result<OwnedWait, String> {
        self.0
            .borrow_mut()
            .push((repo.to_path_buf(), label.to_string(), report.clone()));
        Err("recorded".into())
    }
}

/// A spawner a test must never reach.
struct Unreachable;

impl Spawn for Unreachable {
    fn spawn(
        &self,
        _: &Path,
        _: &AgentLabel,
        _: &str,
        _: Tool,
        _: &Report,
    ) -> Result<OwnedWait, String> {
        panic!("routing should have decided before any report was carried")
    }
}

#[test]
fn the_ceiling_is_the_largest_value_that_does_not_wrap() {
    /// The signed-32-bit cap on the runner's timer delay.
    const MAX_TIMER_MS: u64 = 2_147_483_647;
    let secs = crate::cli::setup::HOOK_TIMEOUT_SECS;
    assert!(
        secs * 1_000 <= MAX_TIMER_MS,
        "{secs}s is {}ms, past the {MAX_TIMER_MS}ms cap — it would wrap and fire at once",
        secs * 1_000
    );
    assert!(
        (secs + 1) * 1_000 > MAX_TIMER_MS,
        "a higher value still fits, so this is not the maximum"
    );
    let started = std::time::Instant::now();
    assert!(poll_deadline(started) > started);
}

#[test]
fn the_deadlines_sit_under_the_runner_ceiling() {
    let started = std::time::Instant::now();
    let ceiling = std::time::Duration::from_secs(crate::cli::setup::HOOK_TIMEOUT_SECS);
    assert!(
        verdict_deadline(poll_deadline(started)) < started + ceiling,
        "the hook stops waiting BEFORE the runner kills it"
    );
    assert_eq!(
        (started + ceiling) - poll_deadline(started),
        std::time::Duration::from_secs(POLL_MARGIN_SECS),
        "the report's deadline leaves exactly the margin"
    );
    assert_eq!(
        verdict_deadline(poll_deadline(started)) - poll_deadline(started),
        VERDICT_GRACE,
        "and the hook waits past it only for the verdict the deadline produces"
    );
    assert_eq!(
        poll_deadline(started),
        poll_deadline(started),
        "the deadline is absolute: later arming gets less time"
    );
}

#[tokio::test(start_paused = true)]
async fn expiry_returns_none_and_drops_the_waiter() {
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = DropFlag(dropped.clone());
    let never = async move {
        let _held = flag;
        std::future::pending::<u8>().await
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    assert_eq!(under_deadline(never, deadline).await, None, "deadline wins");
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn work_arriving_before_the_deadline_wins() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let quick = async {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        "items"
    };
    assert_eq!(under_deadline(quick, deadline).await, Some("items"));
}

#[tokio::test(start_paused = true)]
async fn an_already_past_deadline_does_not_wait_at_all() {
    let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
    assert_eq!(
        under_deadline(std::future::pending::<u8>(), past).await,
        None
    );
}

/// A carried report whose verdict is the future it holds; shutting it
/// down drops that future.
struct Plain<F>(Option<std::pin::Pin<Box<F>>>);

fn plain<F: std::future::Future<Output = Result<Frame, String>>>(f: F) -> Plain<F> {
    Plain(Some(Box::pin(f)))
}

impl<F: std::future::Future<Output = Result<Frame, String>>> Run for Plain<F> {
    async fn verdict_until(&mut self, abandon: impl std::future::Future<Output = ()>) -> Ran {
        let wait = self.0.as_mut().expect("not shut down");
        tokio::pin!(abandon);
        tokio::select! {
            biased;
            () = &mut abandon => Ran::Abandoned,
            frame = wait => Ran::Verdict(frame),
        }
    }

    async fn shutdown(mut self) {
        self.0.take();
    }
}

/// A carried report whose shutdown after abandonment takes `shutdown`.
struct SlowShutdown {
    shutdown: std::time::Duration,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Run for SlowShutdown {
    async fn verdict_until(&mut self, abandon: impl std::future::Future<Output = ()>) -> Ran {
        abandon.await;
        Ran::Abandoned
    }

    async fn shutdown(self) {
        tokio::time::sleep(self.shutdown).await;
        self.done.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn owner_gone<W>(got: &Result<(Frame, W), Frame>) -> bool {
    matches!(
        got,
        Err(Frame {
            verdict: WireVerdict::Silent {
                why: SilentReason::OwnerGone
            },
            ..
        })
    )
}

fn delivered() -> Frame {
    Frame::text(
        WireVerdict::Deliver {
            presentation: PresentationId {
                session: clank_core::SessionId::parse("sess-presented").unwrap(),
                seq: 1,
            },
        },
        "Clank wait returned work".into(),
    )
}

fn pending_wait() -> Plain<impl std::future::Future<Output = Result<Frame, String>>> {
    plain(std::future::pending())
}

#[tokio::test(start_paused = true)]
async fn owner_loss_near_the_deadline_keeps_its_silence_and_its_shutdown() {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mut life, end) = Lifetime::signalled();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let wait = SlowShutdown {
        shutdown: std::time::Duration::from_secs(2),
        done: done.clone(),
    };
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Codex,
        true,
        &mut life,
        &mut crate::upgrade::Replacing::never(),
        || {
            let _ = end.send(());
            Ok(wait)
        },
    )
    .await;
    assert!(owner_gone(&got));
    assert!(
        done.load(std::sync::atomic::Ordering::SeqCst),
        "the shutdown outlived the deadline"
    );
}

#[tokio::test]
async fn an_owner_gone_before_arming_spawns_no_wait() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut armed = false;
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Codex,
        true,
        &mut Lifetime::Gone,
        &mut crate::upgrade::Replacing::never(),
        || {
            armed = true;
            Ok(pending_wait())
        },
    )
    .await;
    assert!(owner_gone(&got));
    assert!(!armed);
}

#[tokio::test]
async fn an_owner_ending_mid_wait_drops_the_wait_silently() {
    struct Dropped(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mut life, end) = Lifetime::signalled();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let guard = Dropped(dropped.clone());
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = end.send(());
    });
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        await_verdict_under_deadline(
            deadline,
            Tool::Codex,
            true,
            &mut life,
            &mut crate::upgrade::Replacing::never(),
            || {
                Ok(plain(async move {
                    let _guard = guard;
                    std::future::pending::<Result<Frame, String>>().await
                }))
            },
        ),
    )
    .await
    .expect("the owner's exit ends the wait at once");
    assert!(owner_gone(&got));
    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "the wait was dropped"
    );
}

#[tokio::test]
async fn owner_loss_outranks_a_verdict_the_wait_already_has() {
    let (mut life, end) = Lifetime::signalled();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Codex,
        true,
        &mut life,
        &mut crate::upgrade::Replacing::never(),
        || {
            Ok(plain(async move {
                let _ = end.send(());
                Ok(delivered())
            }))
        },
    )
    .await;
    assert!(owner_gone(&got));
}

#[tokio::test]
async fn a_live_owner_lets_the_verdict_through() {
    let (mut life, _end) = Lifetime::signalled();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Codex,
        true,
        &mut life,
        &mut crate::upgrade::Replacing::never(),
        || Ok(plain(async { Ok(delivered()) })),
    )
    .await;
    assert!(matches!(got, Ok((ref frame, _)) if frame == &delivered()));
}

/// With no verdict by its deadline the hook frames the one its tool takes
/// at the deadline, so a slow monitor never puts an agent to sleep and
/// OpenCode's plugin renews instead of spinning a turn.
#[tokio::test(start_paused = true)]
async fn the_deadline_re_arms_without_spinning_opencode() {
    for tool in [Tool::Claude, Tool::Codex] {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let got = await_verdict_under_deadline(
            deadline,
            tool,
            true,
            &mut Lifetime::Unbound,
            &mut crate::upgrade::Replacing::never(),
            || Ok(pending_wait()),
        )
        .await;
        let Err(frame) = got else {
            panic!("{tool:?} must re-arm at the deadline");
        };
        assert!(
            matches!(frame.outcome(), HookOutcome::Continue { ref reason } if reason == crate::cli::wake_render::REARM_REASON),
            "{tool:?}: {frame:?}"
        );
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let got = await_verdict_under_deadline(
        deadline,
        Tool::OpenCode,
        true,
        &mut Lifetime::Unbound,
        &mut crate::upgrade::Replacing::never(),
        || Ok(pending_wait()),
    )
    .await;
    assert!(matches!(got, Err(ref f) if f.outcome() == HookOutcome::RenewWait));
}

/// The pre-check: the deadline gone before anything was armed. It re-arms
/// too, and arms nothing it cannot await.
#[tokio::test(start_paused = true)]
async fn the_pre_check_exit_re_arms_without_arming_a_wait() {
    let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let armed = std::cell::Cell::new(false);
    let got = await_verdict_under_deadline(
        past,
        Tool::Claude,
        true,
        &mut Lifetime::Unbound,
        &mut crate::upgrade::Replacing::never(),
        || {
            armed.set(true);
            Ok(pending_wait())
        },
    )
    .await;
    assert!(
        matches!(got, Err(ref f) if matches!(f.outcome(), HookOutcome::Continue { .. })),
        "the pre-check must re-arm too"
    );
    assert!(!armed.get(), "it must short-circuit BEFORE arming");
    let got = await_verdict_under_deadline(
        past,
        Tool::OpenCode,
        true,
        &mut Lifetime::Unbound,
        &mut crate::upgrade::Replacing::never(),
        || Ok(pending_wait()),
    )
    .await;
    assert!(matches!(got, Err(ref f) if f.outcome() == HookOutcome::RenewWait));
}

#[tokio::test(start_paused = true)]
async fn a_verdict_arriving_before_the_deadline_survives_the_seam() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Claude,
        true,
        &mut Lifetime::Unbound,
        &mut crate::upgrade::Replacing::never(),
        || {
            Ok(plain(async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(delivered())
            }))
        },
    )
    .await;
    assert!(matches!(got, Ok((ref f, _)) if f == &delivered()));
}

#[tokio::test(start_paused = true)]
async fn consecutive_expiries_each_re_arm() {
    for period in 0..2 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let got = await_verdict_under_deadline(
            deadline,
            Tool::Claude,
            true,
            &mut Lifetime::Unbound,
            &mut crate::upgrade::Replacing::never(),
            || Ok(pending_wait()),
        )
        .await;
        assert!(
            matches!(got, Err(ref f) if matches!(f.outcome(), HookOutcome::Continue { .. })),
            "period {period} must re-arm"
        );
    }
}

/// A session start never parks: with no verdict by its budget it says
/// nothing at all.
#[tokio::test(start_paused = true)]
async fn a_session_start_out_of_time_says_nothing() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Claude,
        false,
        &mut Lifetime::Unbound,
        &mut crate::upgrade::Replacing::never(),
        || Ok(pending_wait()),
    )
    .await;
    assert!(matches!(got, Err(ref f) if f.verdict == WireVerdict::Park));
}

fn stop_report() -> Report {
    Report::TurnEnd {
        stop: TurnEndReport {
            tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
            hook_started: None,
            tool: Tool::Claude,
            session: clank_core::SessionId::parse("sess-handshake").unwrap(),
            place: Default::default(),
        },
        deadline: u64::MAX,
        questions: Vec::new(),
    }
}

/// The owner handle is held for the whole wait: anything that closed the
/// child's stdin early would give every carried report instant EOF.
#[tokio::test]
async fn the_owner_handle_is_held_for_the_whole_wait() {
    let cmd = Spec::new("/bin/sh")
        .arg("-c")
        .arg("cat >/dev/null")
        .stdin(Stdin::Piped)
        .stdout(Out::Piped)
        .stderr(Out::Piped);
    let mut owned = OwnedWait::spawn(cmd, &stop_report()).expect("sh spawns with a pipe");
    let running = tokio::spawn(async move { owned.verdict_until(std::future::pending()).await });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let exited = running.is_finished();
    running.abort();
    assert!(
        !exited,
        "the child saw EOF while the wait was still running"
    );
}

#[tokio::test]
async fn owner_loss_closes_the_pipe_and_collects_the_wait_after_its_own_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("cleaned-up");
    let cmd = Spec::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "cat >/dev/null; sleep 0.2; touch '{}'",
            marker.display()
        ))
        .stdin(Stdin::Piped)
        .stdout(Out::Piped)
        .stderr(Out::Piped);
    let (mut life, end) = Lifetime::signalled();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = end.send(());
    });
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Codex,
        true,
        &mut life,
        &mut crate::upgrade::Replacing::never(),
        || OwnedWait::spawn(cmd, &stop_report()),
    )
    .await;
    assert!(owner_gone(&got));
    assert!(
        marker.exists(),
        "the child finished its shutdown before the hook returned"
    );
}

/// A shell standing in for the `clank wait --turn-end` child: it reads
/// the report, sends `frames` one per line, then writes whatever result
/// arrives on its stdin — or `eof` — to `out` before it exits.
fn child(frames: &[Frame], out: &Path) -> Spec {
    let lines: Vec<String> = frames
        .iter()
        .map(|f| serde_json::to_string(f).unwrap())
        .collect();
    Spec::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "read report; printf '%s\\n' \"$FRAMES\"; if read result; then printf '%s' \"$result\" \
             > '{0}'; else printf eof > '{0}'; fi",
            out.display()
        ))
        .env("FRAMES", lines.join("\n"))
        .stdin(Stdin::Piped)
        .stdout(Out::Piped)
        .stderr(Out::Piped)
}

/// The hook side of the handshake: a verdict is read while the child is
/// still alive and waiting, a parked verdict is not terminal, and the one
/// result goes back on the same pipe once the flush is known.
#[tokio::test]
async fn a_verdict_is_read_from_a_live_child_and_answered_on_its_pipe() {
    for (flush_fails, want) in [(false, "delivered"), (true, "emit_failed")] {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("result");
        let cmd = child(&[Frame::bare(WireVerdict::Park), delivered()], &out);
        let mut wait = OwnedWait::spawn(cmd, &stop_report()).unwrap();
        let Ran::Verdict(Ok(frame)) = wait.verdict_until(std::future::pending()).await else {
            panic!("a verdict");
        };
        assert_eq!(frame, delivered(), "the parked frame is not terminal");
        assert!(
            wait.child.as_mut().unwrap().try_wait().unwrap().is_none(),
            "the verdict arrived before the child exited"
        );
        let turn = Turn {
            outcome: frame.outcome(),
            presentation: frame.presentation().cloned(),
            binding: None,
            wait: Some(Box::new(wait)),
        };
        let mut stdout = Vec::new();
        let mut stderr = Writer::new();
        stderr.fail_flush = flush_fails;
        let code = emit_turn(turn, Tool::Claude, &mut stdout, &mut stderr).await;
        assert_eq!(code, CLAUDE_CONTINUATION_EXIT);
        let result: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(result["result"], want, "{result}");
        assert_eq!(result["presentation"]["seq"], 1);
    }
}

/// A hook killed before it answers closes its pipe: the child reads EOF
/// with no result, which its monitor takes as a failed emit.
#[tokio::test]
async fn a_killed_hooks_child_reads_eof_and_no_result() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("result");
    let cmd = child(&[delivered()], &out);
    let mut wait = OwnedWait::spawn(cmd, &stop_report()).unwrap();
    let Ran::Verdict(Ok(_)) = wait.verdict_until(std::future::pending()).await else {
        panic!("a verdict");
    };
    let OwnedWait {
        mut child,
        owner_end,
        ..
    } = wait;
    drop(owner_end);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        child.as_mut().unwrap().wait_async(),
    )
    .await
    .expect("the child exits on EOF")
    .unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "eof");
}

/// A child that ends without a terminal verdict is a stated failure that
/// carries what it said.
#[tokio::test]
async fn a_child_that_ends_without_a_verdict_is_diagnostic() {
    let cmd = Spec::new("/bin/sh")
        .arg("-c")
        .arg("read report; echo 'monitor refused' >&2")
        .stdin(Stdin::Piped)
        .stdout(Out::Piped)
        .stderr(Out::Piped);
    let mut wait = OwnedWait::spawn(cmd, &stop_report()).unwrap();
    let Ran::Verdict(Err(message)) = wait.verdict_until(std::future::pending()).await else {
        panic!("a failure");
    };
    assert!(message.contains("without a verdict"), "{message}");
    assert!(message.contains("monitor refused"), "{message}");
}

/// The result a fake wait was answered with.
#[derive(Clone, Default)]
struct Answered(std::sync::Arc<std::sync::Mutex<Option<Option<TurnResult>>>>);

impl Answer for Answered {
    fn answer(
        self: Box<Self>,
        result: Option<TurnResult>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        *self.0.lock().unwrap() = Some(result);
        Box::pin(async {})
    }
}

impl Answered {
    fn got(&self) -> Option<Option<TurnResult>> {
        self.0.lock().unwrap().clone()
    }
}

/// A writer that can fail partway, or at the flush.
struct Writer {
    bytes: Vec<u8>,
    fail_after: Option<usize>,
    fail_flush: bool,
}

impl Writer {
    fn new() -> Self {
        Writer {
            bytes: Vec::new(),
            fail_after: None,
            fail_flush: false,
        }
    }
}

impl std::io::Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let limit = self.fail_after.unwrap_or(usize::MAX);
        if self.bytes.len() >= limit {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let take = bytes.len().min(limit - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..take]);
        Ok(take)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.fail_flush {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        Ok(())
    }
}

fn turn_of(frame: Frame, answered: &Answered) -> Turn {
    Turn {
        outcome: frame.outcome(),
        presentation: frame.presentation().cloned(),
        binding: None,
        wait: Some(Box::new(answered.clone())),
    }
}

/// Each adapter frames the work in its own wire, and the presentation is
/// answered Delivered only once that wire's flush succeeded.
#[tokio::test]
async fn a_delivered_result_follows_a_successful_flush_on_each_adapter() {
    for tool in [Tool::Codex, Tool::Claude, Tool::OpenCode] {
        let answered = Answered::default();
        let mut stdout = Writer::new();
        let mut stderr = Writer::new();
        let code = emit_turn(
            turn_of(delivered(), &answered),
            tool,
            &mut stdout,
            &mut stderr,
        )
        .await;
        assert!(
            matches!(answered.got(), Some(Some(TurnResult::Delivered { .. }))),
            "{tool:?}"
        );
        match tool {
            Tool::Claude => {
                assert_eq!(code, CLAUDE_CONTINUATION_EXIT);
                assert!(stdout.bytes.is_empty());
                assert_eq!(stderr.bytes, b"Clank wait returned work\n");
            }
            Tool::Codex => {
                assert_eq!(code, HOOK_OK_EXIT);
                let value: serde_json::Value = serde_json::from_slice(&stdout.bytes).unwrap();
                assert_eq!(value["decision"], "block");
                assert_eq!(value["reason"], "Clank wait returned work");
            }
            _ => {
                assert_eq!(code, HOOK_OK_EXIT);
                assert_eq!(stdout.bytes, b"Clank wait returned work\n");
            }
        }
    }
}

/// Partial output or a failed flush is a failed emit, never a delivery.
#[tokio::test]
async fn a_failed_or_partial_flush_is_a_failed_emit() {
    for tool in [Tool::Codex, Tool::Claude, Tool::OpenCode] {
        for partial in [true, false] {
            let answered = Answered::default();
            let mut stdout = Writer::new();
            let mut stderr = Writer::new();
            let target = if tool == Tool::Claude {
                &mut stderr
            } else {
                &mut stdout
            };
            if partial {
                target.fail_after = Some(5);
            } else {
                target.fail_flush = true;
            }
            let code = emit_turn(
                turn_of(delivered(), &answered),
                tool,
                &mut stdout,
                &mut stderr,
            )
            .await;
            assert_eq!(
                code,
                if tool == Tool::Claude {
                    CLAUDE_CONTINUATION_EXIT
                } else {
                    HOOK_OK_EXIT
                }
            );
            assert!(
                matches!(answered.got(), Some(Some(TurnResult::EmitFailed { .. }))),
                "{tool:?} partial={partial}"
            );
        }
    }
}

/// The notices a child with no monitor prints really wake, through each
/// adapter's own wire: Claude's continuation exit, Codex's block decision,
/// and OpenCode's prompt rather than its renewal. Each is the child's own
/// frame, carries no presentation and none of the work its repo owes, and
/// is answered with no result, so it earns no receipt.
#[tokio::test(start_paused = true)]
async fn the_childs_no_monitor_notices_wake_through_each_adapter() {
    use crate::cli::turn_child::tests::outage_frame;
    for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
        for (frame, says) in [(
            outage_frame(tool).await,
            "clank cannot start its monitor: starting one failed: no binary.",
        )] {
            assert!(frame.presentation().is_none(), "{tool:?}: {frame:?}");
            let text = frame.rendered.clone().expect("the notice has its text");
            assert!(text.starts_with(says), "{tool:?}: {text}");
            assert!(!text.contains("scheduled work"), "{tool:?}: {text}");
            let answered = Answered::default();
            let mut stdout = Writer::new();
            let mut stderr = Writer::new();
            let code = emit_turn(turn_of(frame, &answered), tool, &mut stdout, &mut stderr).await;
            assert_eq!(answered.got(), Some(None), "{tool:?}: no result to credit");
            match tool {
                Tool::Claude => {
                    assert_eq!(code, CLAUDE_CONTINUATION_EXIT);
                    assert!(stdout.bytes.is_empty());
                    assert_eq!(stderr.bytes, format!("{text}\n").into_bytes());
                }
                Tool::Codex => {
                    assert_eq!(code, HOOK_OK_EXIT);
                    let value: serde_json::Value = serde_json::from_slice(&stdout.bytes).unwrap();
                    assert_eq!(value["decision"], "block");
                    assert_eq!(value["reason"], text);
                }
                _ => {
                    assert_eq!(code, HOOK_OK_EXIT);
                    assert_eq!(stdout.bytes, format!("{text}\n").into_bytes());
                    assert_ne!(
                        stdout.bytes,
                        format!("{OPENCODE_RENEW_WAIT_WIRE}\n").into_bytes(),
                        "a prompt for the model, not a renewal the plugin swallows"
                    );
                }
            }
        }
    }
}

/// The child's ordinary diagnostic is unchanged by all that: on every
/// adapter it goes to stderr with the quiet exit, and wakes nobody.
#[tokio::test]
async fn the_childs_diagnostic_still_wakes_nobody_on_any_adapter() {
    let frame = crate::cli::turn_child::tests::unparsed_frame().await;
    for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
        let answered = Answered::default();
        let mut stdout = Writer::new();
        let mut stderr = Writer::new();
        let code = emit_turn(
            turn_of(frame.clone(), &answered),
            tool,
            &mut stdout,
            &mut stderr,
        )
        .await;
        assert_eq!(code, HOOK_OK_EXIT, "{tool:?}");
        assert!(stdout.bytes.is_empty(), "{tool:?}");
        assert!(
            String::from_utf8_lossy(&stderr.bytes).contains("did not parse"),
            "{tool:?}"
        );
        assert_eq!(answered.got(), Some(None), "{tool:?}");
    }
}

/// What presents nothing is answered with nothing.
#[tokio::test]
async fn a_verdict_that_presents_nothing_answers_nothing() {
    for frame in [
        Frame::bare(WireVerdict::Silent {
            why: SilentReason::StaleBinding,
        }),
        Frame::bare(WireVerdict::Diagnostic {
            message: "diagnostic".into(),
        }),
        Frame::bare(WireVerdict::RenewWait),
        Frame::text(
            WireVerdict::Notify,
            crate::cli::wake_render::MONITOR_UNANSWERED.into(),
        ),
    ] {
        let answered = Answered::default();
        emit_turn(
            turn_of(frame.clone(), &answered),
            Tool::OpenCode,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .await;
        assert_eq!(answered.got(), Some(None), "{frame:?}");
    }
}

/// A binding replaced while the hook waited emits nothing, and the
/// presentation it carried failed.
#[tokio::test]
async fn a_binding_replaced_before_the_emit_sends_nothing() {
    let dir = init_repo();
    let label = bind_tool(dir.path(), "codex", "sess-replaced", Tool::Codex);
    bind_tool(dir.path(), "codex", "sess-newer", Tool::Codex);
    let answered = Answered::default();
    let mut turn = turn_of(delivered(), &answered);
    turn.binding = Some(Binding {
        repo: dir.path().to_path_buf(),
        label,
        tool: Tool::Codex,
        session: "sess-replaced".into(),
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    emit_turn(turn, Tool::Codex, &mut stdout, &mut stderr).await;
    assert!(stdout.is_empty() && stderr.is_empty());
    assert!(matches!(
        answered.got(),
        Some(Some(TurnResult::EmitFailed { .. }))
    ));
}

/// The Stop's evidence is what the report carries: the session, its place,
/// the harness's task listing as Claude sent it (a captured payload), or
/// that it sent none, and the deadline the hook can wait to.
#[tokio::test]
async fn the_report_carries_the_stops_evidence() {
    let dir = init_repo();
    let repo = dunce::canonicalize(dir.path()).unwrap();
    bind_tool(&repo, "claude", "sess-evidence", Tool::Claude);
    let captured: serde_json::Value = serde_json::from_str(include_str!(
        "../../../core/tests/fixtures/stop_hook/stop_subagent_running.json"
    ))
    .unwrap();
    let listed = |payload: &serde_json::Value| -> HookInput {
        let mut payload = payload.clone();
        payload["session_id"] = "sess-evidence".into();
        payload["cwd"] = repo.to_str().unwrap().into();
        serde_json::from_value(payload).unwrap()
    };
    let mut bare = captured.clone();
    bare.as_object_mut().unwrap().remove("background_tasks");
    for (input, tasks) in [
        (
            listed(&captured),
            TaskSnapshot::Known(vec![clank_core::BackgroundTask::from(
                captured["background_tasks"][0].clone(),
            )]),
        ),
        (listed(&bare), TaskSnapshot::Unavailable),
    ] {
        let recorder = Recorder::default();
        let before = super::super::turn_custody::now_ms();
        let turn = stop(
            Tool::Claude,
            Some(&repo),
            arrived(input),
            false,
            &Spawns(&recorder),
        )
        .await;
        assert!(
            matches!(turn.outcome, HookOutcome::Diagnostic { ref message } if message == "recorded")
        );
        let carried = recorder.0.borrow();
        let (
            at,
            who,
            Report::TurnEnd {
                stop: reported,
                deadline,
                ..
            },
        ) = &carried[0]
        else {
            panic!("a Stop's report");
        };
        assert_eq!(at, &repo);
        assert_eq!(who, "claude");
        assert_eq!(reported.tool, Tool::Claude);
        assert_eq!(reported.session.as_str(), "sess-evidence");
        assert_eq!(reported.tasks, tasks);
        assert_eq!(
            reported.hook_started,
            Some(HOOK_STARTED),
            "when its hook started"
        );
        assert!(*deadline > before, "the deadline is ahead of the Stop");
    }
}

#[tokio::test]
async fn unresolved_identity_is_a_diagnostic() {
    let dir = init_repo();
    let turn = stop(
        Tool::Claude,
        Some(dir.path()),
        arrived(hook_input("sess-unbound")),
        false,
        &Spawns(&Unreachable),
    )
    .await;
    assert!(matches!(turn.outcome, HookOutcome::Diagnostic { .. }));
}

/// A linked worktree under `<main>/.clank/worktrees/wt`, as clank makes
/// them, and a directory nested inside it.
fn linked_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let main = init_repo();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(main.path())
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "{args:?}");
    };
    git(&["commit", "--quiet", "--allow-empty", "-m", "init"]);
    let wt = main.path().join(".clank/worktrees/wt");
    git(&["worktree", "add", "--quiet", wt.to_str().unwrap()]);
    let nested = wt.join("docs/split-backup/formal");
    std::fs::create_dir_all(&nested).unwrap();
    (main, dunce::canonicalize(&wt).unwrap(), nested)
}

/// A Stop from inside a linked worktree's subdirectory reports for that
/// worktree's agent: the payload's cwd is where discovery starts, its
/// containing worktree is the repo, and neither the main checkout nor the
/// hook process's own cwd is consulted.
#[tokio::test]
async fn a_stop_from_a_worktree_subdirectory_reports_for_that_worktree() {
    let (main, wt, nested) = linked_worktree();
    bind_tool(&wt, "claude", "sess-nested", Tool::Claude);
    let input: HookInput = serde_json::from_value(serde_json::json!({
        "session_id": "sess-nested",
        "cwd": nested.to_str().unwrap(),
        "stop_hook_active": false,
    }))
    .unwrap();
    let recorder = Recorder::default();
    stop(
        Tool::Claude,
        None,
        arrived(input.clone()),
        false,
        &Spawns(&recorder),
    )
    .await;
    assert_eq!(
        recorder.0.borrow()[0].0,
        wt,
        "the worktree, not the main checkout"
    );

    let link = main.path().join("formal-link");
    std::os::unix::fs::symlink(&nested, &link).unwrap();
    assert_eq!(resolve_cwd_repo(&link).unwrap(), wt);
    assert_eq!(
        session_start_repo(None, nested.to_str().unwrap()).unwrap(),
        wt,
        "SessionStart binds in the worktree holding its cwd"
    );
    assert_eq!(
        session_start_repo(Some(&nested), nested.to_str().unwrap()).unwrap(),
        dunce::canonicalize(&nested).unwrap()
    );
    assert_eq!(
        resolve_hook_repo(Some(&nested), &input).unwrap(),
        dunce::canonicalize(&nested).unwrap(),
        "an explicit --repo is exact, not discovered from"
    );
}

/// A Claude Stop ends its own turn's questions and those of the turns
/// behind it; other harnesses ask none.
#[test]
fn a_stop_proves_which_turns_are_over() {
    use super::super::question_state::Observation;
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("t.jsonl");
    let user =
        |id: &str| format!(r#"{{"type":"user","promptId":"{id}","message":{{"content":"go"}}}}"#);
    std::fs::write(&transcript, format!("{}\n{}\n", user("p1"), user("p2"))).unwrap();
    let mut input = hook_input("session-one");
    input.transcript_path = Some(transcript.display().to_string());
    let place = super::super::wake_repeat::place(Tool::Claude, Some(&transcript), None);
    assert_eq!(
        question_evidence(Tool::Claude, &input, &place),
        vec![
            Observation::TurnOver {
                session: "session-one".into(),
                prompt: "p2".into(),
            },
            Observation::Behind {
                session: "session-one".into(),
                older: vec!["p1".into()]
            },
        ]
    );
    assert!(question_evidence(Tool::Codex, &input, &place).is_empty());
    let unplaced = TurnPlace {
        position: None,
        ..place
    };
    assert!(
        question_evidence(Tool::Claude, &input, &unplaced).is_empty(),
        "a turn named without its boundary proves nothing of its questions"
    );
}

#[test]
fn the_child_carries_the_report_and_dies_with_its_owner() {
    let label = AgentLabel::parse("claude").unwrap();
    let argv = wait_argv(Path::new("/repo"), &label, "sess-argv");
    for flag in ["--die-with-owner", "--turn-end", "--deliver-to-session"] {
        assert!(argv.iter().any(|a| a == flag), "{flag}: {argv:?}");
    }
    assert!(
        !argv.iter().any(|a| a == "--role"),
        "the monitor derives the role"
    );
}

fn start_input(session: &str, source: &str) -> SessionStartInput {
    SessionStartInput {
        session_id: session.to_string(),
        cwd: "/tmp".to_string(),
        source: Some(source.to_string()),
        transcript_path: None,
    }
}

fn bound_session(repo: &Path, label: &str) -> Option<(Tool, String)> {
    let label = AgentLabel::parse(label).unwrap();
    crate::agent_store::load_agent_config(repo, &label)
        .unwrap()
        .and_then(|c| c.session)
        .map(|s| (s.tool, s.id.as_str().to_string()))
}

fn roster_agent(repo: &Path, label: &AgentLabel, tool: Tool) {
    crate::cli::agent::add_repo_roster_agent(
        repo,
        label,
        crate::cli::teams_config::AgentDescription {
            tool,
            launch: None,
            initial_prompt: None,
        },
        crate::cli::teams_config::RosterRole::Master,
    )
    .unwrap();
}

/// A bound session keeps its binding whatever label its launch names:
/// only an unbound session is bootstrapped.
#[test]
fn a_bound_session_keeps_its_label_at_start() {
    let dir = init_repo();
    let repo = dir.path();
    let sid = "01a08361-f112-75d0-a49f-2e35b104e3b6";
    bind_tool(repo, "old", sid, Tool::Codex);
    let new = AgentLabel::parse("new").unwrap();
    roster_agent(repo, &new, Tool::Codex);
    let got = session_started(repo, Tool::Codex, &start_input(sid, "startup"), Some(&new));
    assert_eq!(got.as_ref().map(|(l, _)| l.as_str()), Some("old"));
    assert_eq!(
        bound_session(repo, "old"),
        Some((Tool::Codex, sid.to_string()))
    );
    assert_eq!(bound_session(repo, "new"), None);
}

/// story-short, 2026-10-24: `codex` is bound to the reviewer R and
/// `painter` to P when an unbound session U starts in the shared codex
/// daemon, which inherited `CLANK_AGENT=codex`. That hook does not run in
/// its launch, so U binds nothing, both bindings stay, and R and P are
/// still themselves.
#[test]
fn an_unbound_session_under_the_daemon_takes_no_label() {
    let dir = init_repo();
    let repo = dir.path();
    let (r, p, u) = (
        "01a16cdd-5f28-7bd2-b895-7122737638ce",
        "01a16cfb-9a15-7801-a3de-671cc0308ebb",
        "01a16cfc-2e23-7352-875f-d74f16dc1305",
    );
    bind_tool(repo, "codex", r, Tool::Codex);
    bind_tool(repo, "painter", p, Tool::Codex);
    let daemon = crate::agent_env::Stated::saying(&[("CLANK_AGENT", "codex")]);
    let launched_as = launch_label(Tool::Codex, &daemon, &Parent::daemon());
    assert_eq!(launched_as, None);
    assert_eq!(
        session_started(
            repo,
            Tool::Codex,
            &start_input(u, "startup"),
            launched_as.as_ref()
        ),
        None
    );
    assert_eq!(bound_session(repo, "codex"), Some((Tool::Codex, r.into())));
    assert_eq!(
        bound_session(repo, "painter"),
        Some((Tool::Codex, p.into()))
    );
    for (sid, who) in [(r, "codex"), (p, "painter")] {
        let got = session_started(
            repo,
            Tool::Codex,
            &start_input(sid, "resume"),
            launched_as.as_ref(),
        );
        assert_eq!(got.as_ref().map(|(l, _)| l.as_str()), Some(who));
        let id = clank_core::SessionId::parse(sid).unwrap();
        assert_eq!(
            crate::agent_env::resolve_identity_for_hook_in(repo, Tool::Codex, &id)
                .unwrap()
                .as_str(),
            who
        );
    }
}

/// The launch's label reaches a codex SessionStart only from a standalone
/// client's own hook, and any other tool's hook always carries it.
#[test]
fn only_a_hook_in_its_launch_carries_the_launch_label() {
    let named = crate::agent_env::Stated::saying(&[("CLANK_AGENT", "painter")]);
    let painter = Some(AgentLabel::parse("painter").unwrap());
    assert_eq!(
        launch_label(Tool::Codex, &named, &Parent::standalone()),
        painter
    );
    assert_eq!(launch_label(Tool::Codex, &named, &Parent::daemon()), None);
    assert_eq!(
        launch_label(Tool::Codex, &named, &Parent::unreadable()),
        None
    );
    for tool in [Tool::Claude, Tool::Grok, Tool::OpenCode] {
        assert_eq!(
            launch_label(tool, &named, &Parent::daemon()),
            painter,
            "{tool:?}"
        );
    }
    assert_eq!(
        launch_label(Tool::Codex, &stated(), &Parent::standalone()),
        None
    );
}

/// A parent process as a codex hook would find it.
struct Parent(crate::cli::hook_owner::Argv);

impl Parent {
    fn standalone() -> Self {
        Parent(crate::cli::hook_owner::Argv::Read(vec![
            "/usr/local/bin/codex".into(),
            "--no-daemon".into(),
        ]))
    }
    fn daemon() -> Self {
        Parent(crate::cli::hook_owner::Argv::Read(vec![
            "/Users/x/.codex/packages/app-server-daemon/bin/codex".into(),
            "app-server".into(),
            "--listen".into(),
            "unix://".into(),
        ]))
    }
    fn unreadable() -> Self {
        Parent(crate::cli::hook_owner::Argv::Unreadable)
    }
}

impl crate::cli::hook_owner::Parentage for Parent {
    fn parent(&self) -> i32 {
        4242
    }
    fn token(&self, _pid: i32) -> Option<crate::proc_identity::ProcToken> {
        crate::proc_identity::token_for(std::process::id() as i32)
    }
    fn argv(
        &self,
        _pid: i32,
        _birth: &crate::proc_identity::ProcToken,
    ) -> crate::cli::hook_owner::Argv {
        self.0.clone()
    }
}

/// A Grok running Claude-compatible hooks enters as claude. Its id is
/// already bound under grok, so its start binds nothing whichever label
/// it inherited, a claude agent's included, and every binding stays put.
/// Grok's native hook binds it.
#[test]
fn a_compatible_grok_start_binds_nothing() {
    let dir = init_repo();
    let repo = dir.path();
    let grok = AgentLabel::parse("grok").unwrap();
    roster_agent(repo, &grok, Tool::Grok);
    let codex = AgentLabel::parse("codex").unwrap();
    crate::cli::agent::add_repo_roster_agent(
        repo,
        &codex,
        crate::cli::teams_config::AgentDescription {
            tool: Tool::Codex,
            launch: None,
            initial_prompt: None,
        },
        crate::cli::teams_config::RosterRole::Commit,
    )
    .unwrap();
    let claude = AgentLabel::parse("claude").unwrap();
    crate::cli::agent::add_repo_roster_agent(
        repo,
        &claude,
        crate::cli::teams_config::AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
        crate::cli::teams_config::RosterRole::Commit,
    )
    .unwrap();
    for (label, tool) in [
        (&grok, Tool::Grok),
        (&codex, Tool::Codex),
        (&claude, Tool::Claude),
    ] {
        assert_eq!(crate::agent_store::resolve_tool(repo, label).unwrap(), tool);
    }
    let (p, r, c) = (
        "01a0dd00-5b0f-7dc0-b418-24054b5112cf",
        "01a16cdd-5f28-7bd2-b895-7122737638ce",
        "742f6a04-f174-409a-ab01-419a16c5f372",
    );
    bind_tool(repo, "painter", p, Tool::Grok);
    bind_tool(repo, "codex", r, Tool::Codex);
    bind_tool(repo, "claude", c, Tool::Claude);
    for inherited in [&grok, &codex, &claude] {
        let mut input = start_input(p, "resume");
        input.transcript_path = Some("/tmp/.grok/updates.jsonl".into());
        assert_eq!(
            session_started(repo, Tool::Claude, &input, Some(inherited)),
            None,
            "{inherited}"
        );
        assert_eq!(bound_session(repo, "painter"), Some((Tool::Grok, p.into())));
        assert_eq!(bound_session(repo, "codex"), Some((Tool::Codex, r.into())));
        assert_eq!(
            bound_session(repo, "claude"),
            Some((Tool::Claude, c.into()))
        );
        assert_eq!(bound_session(repo, "grok"), None);
    }
}

/// No label in the environment: a session clank did not launch. It is
/// identified by its id as `clank as` bound it, and an unbound id binds
/// nothing.
#[test]
fn a_hand_started_session_is_identified_by_its_id_and_never_bound_here() {
    let dir = init_repo();
    let repo = dir.path();
    let sid = "9c96eb03-1458-4bac-abbb-90243cfd422c";
    assert!(session_started(repo, Tool::Claude, &start_input(sid, "startup"), None,).is_none());
    assert!(
        crate::agent_store::load_all_agent_configs(repo)
            .unwrap()
            .is_empty()
    );
    bind_tool(repo, "claude", sid, Tool::Claude);
    let got = session_started(repo, Tool::Claude, &start_input(sid, "clear"), None);
    assert_eq!(got.as_ref().map(|(l, _)| l.as_str()), Some("claude"));
}

#[test]
fn session_start_payload_carries_items_as_additional_context() {
    let v = session_start_payload("Clank wait returned work for `x`.");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.starts_with("Pending clank work found at session start."));
    assert!(ctx.contains("Clank wait returned work"), "{ctx}");
}

/// A session start's work rides `additionalContext`, answered Delivered
/// only once its flush is known; a notice rides it too and answers
/// nothing; and nothing is said when nothing is owed.
#[tokio::test]
async fn a_session_starts_context_is_answered_after_its_flush() {
    let dir = init_repo();
    let label = bind_tool(dir.path(), "claude", "sess-start", Tool::Claude);
    let binding = Binding {
        repo: dir.path().to_path_buf(),
        label,
        tool: Tool::Claude,
        session: "sess-start".into(),
    };
    for failure in [false, true] {
        let answered = Answered::default();
        let mut writer = Writer::new();
        writer.fail_flush = failure;
        emit_session_start(delivered(), &binding, answered.clone(), &mut writer).await;
        let value: serde_json::Value = serde_json::from_slice(&writer.bytes).unwrap();
        assert_eq!(value, session_start_payload("Clank wait returned work"));
        match answered.got() {
            Some(Some(TurnResult::Delivered { .. })) => assert!(!failure),
            Some(Some(TurnResult::EmitFailed { .. })) => assert!(failure),
            other => panic!("{other:?}"),
        }
    }
    let answered = Answered::default();
    let mut writer = Writer::new();
    emit_session_start(
        Frame::text(
            WireVerdict::Notify,
            crate::cli::wake_render::monitor_outage("no binary"),
        ),
        &binding,
        answered.clone(),
        &mut writer,
    )
    .await;
    assert!(String::from_utf8_lossy(&writer.bytes).contains("cannot start its monitor"));
    assert_eq!(answered.got(), Some(None));
    let answered = Answered::default();
    let mut writer = Writer::new();
    emit_session_start(
        Frame::bare(WireVerdict::Park),
        &binding,
        answered.clone(),
        &mut writer,
    )
    .await;
    assert!(writer.bytes.is_empty());
    assert_eq!(answered.got(), Some(None));
}

fn arm_resume_of(repo: &Path, label: &str, session: &str) {
    let label = AgentLabel::parse(label).unwrap();
    let sid = clank_core::ids::SessionId::parse(session).unwrap();
    crate::agent_store::record_transcript(repo, &label, &sid, "/t/a.jsonl").unwrap();
    assert!(
        crate::agent_store::arm_resume(repo, &label, &sid, time::OffsetDateTime::now_utc())
            .unwrap()
    );
}

fn resume_pending(repo: &Path, label: &str) -> Option<String> {
    let label = AgentLabel::parse(label).unwrap();
    crate::agent_store::load_agent_config(repo, &label)
        .unwrap()
        .and_then(|c| c.resume_pending)
        .map(|p| p.session.as_str().to_string())
}

#[test]
fn a_resumed_claude_session_checks_in_at_its_start() {
    let dir = init_repo();
    let repo = dir.path();
    let sid = "742f6a04-f174-409a-ab01-419a16c5f372";
    bind_tool(repo, "claude", sid, Tool::Claude);
    arm_resume_of(repo, "claude", sid);
    let got = session_started(repo, Tool::Claude, &start_input(sid, "resume"), None);
    assert_eq!(got.as_ref().map(|(l, _)| l.as_str()), Some("claude"));
    assert_eq!(resume_pending(repo, "claude"), None);
}

/// The marker is for the session the label was bound to when it was
/// armed; the session bound since checking in says nothing about that one.
#[test]
fn another_sessions_check_in_leaves_the_marker() {
    let dir = init_repo();
    let repo = dir.path();
    let (armed, successor) = (
        "742f6a04-f174-409a-ab01-419a16c5f372",
        "0f3e5c2a-1b4d-4e6f-8a9b-0c1d2e3f4a5b",
    );
    bind_tool(repo, "claude", armed, Tool::Claude);
    arm_resume_of(repo, "claude", armed);
    bind_tool(repo, "claude", successor, Tool::Claude);
    let got = session_started(repo, Tool::Claude, &start_input(successor, "resume"), None);
    assert_eq!(got.as_ref().map(|(l, _)| l.as_str()), Some("claude"));
    assert_eq!(resume_pending(repo, "claude"), Some(armed.into()));
}

#[test]
fn a_bootstrap_start_leaves_the_marker() {
    let dir = init_repo();
    let repo = dir.path();
    let (armed, fresh) = (
        "742f6a04-f174-409a-ab01-419a16c5f372",
        "0f3e5c2a-1b4d-4e6f-8a9b-0c1d2e3f4a5b",
    );
    let claude = bind_tool(repo, "claude", armed, Tool::Claude);
    roster_agent(repo, &claude, Tool::Claude);
    arm_resume_of(repo, "claude", armed);
    crate::agent_store::release_binding(
        repo,
        &claude,
        &clank_core::ids::SessionId::parse(armed).unwrap(),
    )
    .unwrap();
    let got = session_started(
        repo,
        Tool::Claude,
        &start_input(fresh, "startup"),
        Some(&claude),
    );
    assert_eq!(got.as_ref().map(|(l, _)| l.as_str()), Some("claude"));
    assert_eq!(
        bound_session(repo, "claude"),
        Some((Tool::Claude, fresh.into()))
    );
    assert_eq!(resume_pending(repo, "claude"), Some(armed.into()));
}

/// A [`crate::upgrade::Replacing`] over `installed` whose every attempt
/// fails, counted in `tried`.
fn failing<'a>(
    installed: &crate::upgrade::tests::Installed,
    tried: &'a std::cell::Cell<usize>,
) -> crate::upgrade::Replacing<impl FnMut(&crate::build_stamp::Build) -> std::io::Error> {
    crate::upgrade::Replacing::new(installed.authority(), move |_| {
        tried.set(tried.get() + 1);
        std::io::Error::other("the binary is missing mid-install")
    })
}

/// A parked hook under a newer install tries to become it on a beat. Each
/// attempt that fails leaves the park exactly as it was: still held, the
/// attempt repeated, and the verdict that finally comes is the one it
/// frames.
#[tokio::test(start_paused = true)]
async fn a_parked_hook_whose_replacement_fails_stays_parked_and_retries() {
    let installed = crate::upgrade::tests::Installed::current();
    installed.replace(1);
    let tried = std::cell::Cell::new(0);
    let mut replacing = failing(&installed, &tried);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Claude,
        true,
        &mut Lifetime::Unbound,
        &mut replacing,
        || {
            Ok(plain(async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok(delivered())
            }))
        },
    )
    .await;
    assert_eq!(got.ok().map(|(frame, _)| frame), Some(delivered()));
    assert!(tried.get() > 3, "retried through the park: {}", tried.get());
}

/// With its own build still installed a parked hook asks for nothing.
#[tokio::test(start_paused = true)]
async fn a_parked_hook_of_the_installed_build_is_left_alone() {
    let installed = crate::upgrade::tests::Installed::current();
    let tried = std::cell::Cell::new(0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Claude,
        true,
        &mut Lifetime::Unbound,
        &mut failing(&installed, &tried),
        || {
            Ok(plain(async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok(delivered())
            }))
        },
    )
    .await;
    assert!(got.is_ok());
    assert_eq!(tried.get(), 0);
}

/// A verdict already in hand goes first: a hook holding a presentation
/// frames it and answers for it as the build that received it, however
/// stale that build is.
#[tokio::test(start_paused = true)]
async fn a_hook_with_a_verdict_in_hand_is_not_replaced() {
    let installed = crate::upgrade::tests::Installed::current();
    installed.replace(1);
    let tried = std::cell::Cell::new(0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let got = await_verdict_under_deadline(
        deadline,
        Tool::Claude,
        true,
        &mut Lifetime::Unbound,
        &mut failing(&installed, &tried),
        || Ok(plain(async { Ok(delivered()) })),
    )
    .await;
    assert_eq!(got.ok().map(|(frame, _)| frame), Some(delivered()));
    assert_eq!(tried.get(), 0, "mid-presentation is not a safe point");
}

/// A host of a build that has been replaced: it starts carriers over
/// pipes the test holds, and records each handoff it is asked to resume
/// the installed build from, which it never becomes.
struct Replaced {
    authority: crate::upgrade::Authority,
    /// Each carrier's report, its owner pipe, and its verdict pipe until
    /// the test takes that to speak as the carrier.
    carriers: std::cell::RefCell<Vec<Carrier>>,
    handoffs:
        std::cell::RefCell<Vec<(PathBuf, Result<super::super::hook_handoff::Handoff, String>)>>,
}

struct Carrier {
    report: Report,
    _owner_pipe: tokio::io::DuplexStream,
    verdicts: Option<tokio::io::DuplexStream>,
}

impl Spawn for Replaced {
    fn spawn(
        &self,
        _: &Path,
        _: &AgentLabel,
        _: &str,
        _: Tool,
        report: &Report,
    ) -> Result<OwnedWait, String> {
        let (owner_end, owner_read) = tokio::io::duplex(64 * 1024);
        let (frames_write, frames) = tokio::io::duplex(64 * 1024);
        self.carriers.borrow_mut().push(Carrier {
            report: report.clone(),
            _owner_pipe: owner_read,
            verdicts: Some(frames_write),
        });
        OwnedWait::over(None, Box::new(owner_end), Box::new(frames), None, report)
    }
}

impl Host for Replaced {
    fn now_ms(&self) -> Ms {
        HOOK_STARTED
    }
    fn input(&mut self) -> Result<String, String> {
        unreachable!("the input is supplied")
    }
    fn spawner(&self) -> &dyn Spawn {
        self
    }
    fn streams(&mut self) -> (&mut dyn std::io::Write, &mut dyn std::io::Write) {
        unreachable!("nothing is emitted through the host")
    }
    fn exit(&mut self, _: i32) {}
    fn authority(&self) -> crate::upgrade::Authority {
        self.authority.clone()
    }
    fn become_installed(&self, handoff: &Path) -> std::io::Error {
        let read = std::fs::read(handoff)
            .map_err(|e| e.to_string())
            .and_then(|bytes| super::super::hook_handoff::Handoff::parse(&bytes));
        self.handoffs
            .borrow_mut()
            .push((handoff.to_path_buf(), read));
        std::io::Error::other("the binary is missing mid-install")
    }
}

/// The whole Stop under a newer install. While parked it offers the
/// installed build a handoff holding its input byte for byte and the very
/// report its carrier was armed with; each failed attempt leaves no file,
/// no second carrier and nothing emitted; and the presentation that then
/// arrives is the Stop's answer.
#[tokio::test(start_paused = true)]
async fn a_stale_parked_stop_hands_over_its_input_and_report_and_survives_failure() {
    use tokio::io::AsyncWriteExt;
    let repo = init_repo();
    bind_tool(repo.path(), "claude", "sess-moving", Tool::Claude);
    let raw = serde_json::json!({
        "session_id": "sess-moving",
        "cwd": repo.path(),
        "stop_hook_active": false,
        "an_extra_the_harness_sent": [1, 2, 3],
    })
    .to_string();
    let installed = crate::upgrade::tests::Installed::current();
    installed.replace(1);
    let host = Replaced {
        authority: installed.authority(),
        carriers: Default::default(),
        handoffs: Default::default(),
    };
    let arrived = Arrived::fresh(raw.clone(), std::time::Instant::now(), HOOK_STARTED).unwrap();
    let presents = async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let line = serde_json::to_string(&delivered()).unwrap();
        let mut verdicts = host.carriers.borrow_mut()[0]
            .verdicts
            .take()
            .expect("the one carrier");
        verdicts
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    };
    let (turn, ()) = tokio::join!(
        stop(Tool::Claude, Some(repo.path()), arrived, false, &host),
        presents
    );

    assert_eq!(turn.presentation, delivered().presentation().cloned());
    assert!(
        turn.wait.is_some(),
        "the carrier that presented answers for it"
    );
    let carriers = host.carriers.borrow();
    assert_eq!(carriers.len(), 1, "one carrier throughout");
    let handoffs = host.handoffs.borrow();
    assert!(handoffs.len() > 1, "a failed attempt is made again");
    for (path, read) in handoffs.iter() {
        let handoff = read.as_ref().expect("the installed build can read it");
        assert_eq!(handoff.input, raw);
        assert_eq!(handoff.report, carriers[0].report);
        assert!(!path.exists(), "a failed attempt leaves its handoff behind");
    }
}

/// The installed build, resumed from a handoff, carries the report it was
/// handed and not one read again now: the same evidence, the same
/// deadline, and a wait held no longer than the first build would have.
#[tokio::test]
async fn a_resumed_stop_carries_the_report_it_was_handed() {
    let repo = init_repo();
    bind_tool(repo.path(), "claude", "sess-resumed", Tool::Claude);
    let raw = serde_json::json!({
        "session_id": "sess-resumed",
        "cwd": repo.path(),
        "stop_hook_active": false,
    })
    .to_string();
    let left = std::time::Duration::from_secs(600);
    let report = Report::TurnEnd {
        stop: TurnEndReport {
            tool: Tool::Claude,
            session: clank_core::SessionId::parse("sess-resumed").unwrap(),
            place: TurnPlace {
                turn: Some("the-turn-the-first-build-read".into()),
                position: Some(4242),
                human_turn: true,
                read_to: None,
            },
            tasks: TaskSnapshot::Unavailable,
            hook_started: None,
        },
        deadline: super::super::turn_custody::now_ms() + left.as_millis() as u64,
        questions: Vec::new(),
    };
    let handoff = super::super::hook_handoff::Handoff::new(raw, report.clone());
    let arrived = Arrived::resumed(handoff).unwrap();
    let until = arrived
        .poll_deadline
        .saturating_duration_since(std::time::Instant::now());
    assert!(
        until <= left && until > left - std::time::Duration::from_secs(5),
        "{until:?}"
    );
    let recorder = Recorder::default();
    stop(
        Tool::Claude,
        Some(repo.path()),
        arrived,
        false,
        &Spawns(&recorder),
    )
    .await;
    assert_eq!(recorder.0.borrow()[0].2, report);

    let Report::TurnEnd { stop, .. } = report else {
        unreachable!()
    };
    let unbounded = super::super::hook_handoff::Handoff::new(
        serde_json::json!({"session_id": "sess-resumed", "cwd": repo.path()}).to_string(),
        Report::Compacted {
            stop,
            deadline: u64::MAX,
        },
    );
    let arrived = Arrived::resumed(unbounded).unwrap();
    assert!(
        arrived.poll_deadline <= poll_deadline(std::time::Instant::now()),
        "a handoff cannot keep a hook past the runner's ceiling"
    );
}

/// A payload recorded under `core/tests/fixtures/stop_hook/`, as Claude
/// would send it for `session` working in `repo`.
fn recorded(fixture: &str, repo: &Path, session: &str) -> serde_json::Value {
    let path = format!(
        "{}/../core/tests/fixtures/stop_hook/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = session.into();
    payload["cwd"] = repo.to_str().unwrap().into();
    payload
}

/// Claude's documented `SubagentStart` payload: the common fields plus
/// `agent_id` and `agent_type`. None was recorded.
fn documented_start(repo: &Path, session: &str, child: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": session,
        "transcript_path": "/REDACTED/transcript.jsonl",
        "cwd": repo,
        "hook_event_name": "SubagentStart",
        "agent_id": child,
        "agent_type": "general-purpose",
    })
}

const RECORDED_A: &str = "a2185a1add00531a0";

/// A subagent's stop reports the session's listing as Claude sent it, the
/// agent's own record as the hook read it, and when the hook started; its
/// start reports only that. Either is a report that does not park, holds
/// nothing for its session and is given seconds, not a park's ceiling. The
/// agent's own Stop sent this way reports nothing, and neither does a
/// session that no longer holds the label, which is told why on stderr as
/// its Stop would be.
#[tokio::test]
async fn a_subagents_hook_reports_what_it_saw_and_never_parks() {
    use super::super::SubagentArg;
    let dir = init_repo();
    let repo = dunce::canonicalize(dir.path()).unwrap();
    bind_tool(&repo, "claude", "sess-child", Tool::Claude);
    let transcript = repo.join("agent.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"user\",\"promptId\":\"prompt-1\",\"message\":{\"content\":\"go\"}}\n",
    )
    .unwrap();
    let mut stops = recorded("subagent_stop_a_parks.json", &repo, "sess-child");
    stops["transcript_path"] = transcript.to_str().unwrap().into();
    let listed: Vec<clank_core::BackgroundTask> =
        serde_json::from_value(stops["background_tasks"].clone()).unwrap();
    assert_eq!(listed.len(), 4);
    let agent = super::super::wake_repeat::place(Tool::Claude, Some(&transcript), None);
    assert_eq!(agent.turn.as_deref(), Some("prompt-1"));

    for (event, payload, seen) in [
        (
            SubagentArg::Stop,
            stops,
            ChildSeen::Stopped {
                tasks: TaskSnapshot::Known(listed),
                agent,
            },
        ),
        (
            SubagentArg::Start,
            documented_start(&repo, "sess-child", RECORDED_A),
            ChildSeen::Started,
        ),
    ] {
        let recorder = Recorder::default();
        let before = super::super::turn_custody::now_ms();
        let said = subagent(
            Tool::Claude,
            Some(&repo),
            event,
            &payload.to_string(),
            HOOK_STARTED,
            &Spawns(&recorder),
        )
        .await;
        assert_eq!(said.as_deref(), Some("recorded"));
        let carried = recorder.0.borrow();
        let [(at, who, report)] = carried.as_slice() else {
            panic!("one report: {carried:?}");
        };
        assert_eq!((at, who.as_str()), (&repo, "claude"));
        assert!(!report.parks() && !report.holds_session() && !report.reaches_agent());
        let left = report.deadline().saturating_sub(before);
        assert!(
            left > 0 && left <= SUBAGENT_BUDGET.as_millis() as u64 + 1000,
            "{left}ms"
        );
        assert_eq!(
            report,
            &Report::Child {
                tool: Tool::Claude,
                seen: ChildReport {
                    session: clank_core::SessionId::parse("sess-child").unwrap(),
                    child: RECORDED_A.into(),
                    hook_started: Some(HOOK_STARTED),
                    seen,
                },
                deadline: report.deadline(),
            }
        );
    }

    let agents_own = recorded("stop_a_parked_b_running.json", &repo, "sess-child");
    let said = subagent(
        Tool::Claude,
        Some(&repo),
        SubagentArg::Stop,
        &agents_own.to_string(),
        HOOK_STARTED,
        &Spawns(&Unreachable),
    )
    .await;
    assert_eq!(said, None, "it names no subagent");

    bind_tool(&repo, "claude", "sess-newer", Tool::Claude);
    let stale = recorded("subagent_stop_a_final.json", &repo, "sess-child");
    let said = subagent(
        Tool::Claude,
        Some(&repo),
        SubagentArg::Stop,
        &stale.to_string(),
        HOOK_STARTED,
        &Spawns(&Unreachable),
    )
    .await;
    assert!(
        said.as_deref()
            .is_some_and(|why| why.contains("no agent set up")),
        "another session holds the label: {said:?}"
    );
}

/// Nothing a subagent's hook reports is read from the subagent's own
/// transcript: missing, written, replaced by another file or truncated,
/// the report is the same.
#[test]
fn a_subagents_report_reads_nothing_of_its_own_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let own = dir.path().join("agent-a.jsonl");
    let mut payload = recorded("subagent_stop_a_parks.json", dir.path(), "sess-child");
    payload["agent_transcript_path"] = own.to_str().unwrap().into();
    let report = |payload: &serde_json::Value| {
        subagent_report(
            Tool::Claude,
            super::super::SubagentArg::Stop,
            &payload.to_string(),
            HOOK_STARTED,
        )
        .unwrap()
        .expect("it names a subagent")
        .1
    };
    let missing = report(&payload);
    std::fs::write(&own, "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n").unwrap();
    assert_eq!(report(&payload), missing, "written");
    std::fs::write(&own, "").unwrap();
    assert_eq!(report(&payload), missing, "truncated");
    payload["agent_transcript_path"] = "/elsewhere/agent-b.jsonl".into();
    assert_eq!(report(&payload), missing, "replaced");
}

/// A host fed one payload, whose carrier answers with `frames` and then
/// stays silent with its pipe open.
struct Fed {
    stdin: String,
    answers: Vec<Frame>,
    carried: std::cell::RefCell<Vec<Report>>,
    held: std::cell::RefCell<Vec<tokio::io::DuplexStream>>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: Option<i32>,
}

impl Fed {
    fn new(stdin: String, answers: Vec<Frame>) -> Self {
        Fed {
            stdin,
            answers,
            carried: Default::default(),
            held: Default::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: None,
        }
    }
}

impl Spawn for Fed {
    fn spawn(
        &self,
        _: &Path,
        _: &AgentLabel,
        _: &str,
        _: Tool,
        report: &Report,
    ) -> Result<OwnedWait, String> {
        self.carried.borrow_mut().push(report.clone());
        let said: String = self
            .answers
            .iter()
            .map(|frame| format!("{}\n", serde_json::to_string(frame).unwrap()))
            .collect();
        let (silence, open) = tokio::io::duplex(64);
        self.held.borrow_mut().push(silence);
        OwnedWait::over(
            None,
            Box::new(tokio::io::sink()),
            Box::new(tokio::io::AsyncReadExt::chain(
                std::io::Cursor::new(said.into_bytes()),
                open,
            )),
            None,
            report,
        )
    }
}

impl Host for Fed {
    fn now_ms(&self) -> Ms {
        HOOK_STARTED
    }
    fn input(&mut self) -> Result<String, String> {
        Ok(self.stdin.clone())
    }
    fn spawner(&self) -> &dyn Spawn {
        self
    }
    fn streams(&mut self) -> (&mut dyn std::io::Write, &mut dyn std::io::Write) {
        (&mut self.stdout, &mut self.stderr)
    }
    fn exit(&mut self, code: i32) {
        self.exit = Some(code);
    }
    fn authority(&self) -> crate::upgrade::Authority {
        crate::upgrade::Authority::unwatched()
    }
    fn become_installed(&self, _: &Path) -> std::io::Error {
        panic!("a subagent's hook is never replaced: it does not park")
    }
}

/// The whole `--subagent` hook, from its argv: acknowledged, it exits 0
/// at once having written nothing. Left unanswered by a carrier that
/// stays connected, or told to park, it is still gone within its budget,
/// exit 0 and silent: it cannot hold a subagent's stop. Only a diagnostic
/// is written, to stderr, and that exits 0 too.
#[tokio::test(start_paused = true)]
async fn a_subagents_hook_exits_zero_and_silent_within_its_budget() {
    let dir = init_repo();
    let repo = dunce::canonicalize(dir.path()).unwrap();
    bind_tool(&repo, "claude", "sess-child", Tool::Claude);
    let args = |event: &str| {
        let argv = [
            "clank",
            "stop-hook",
            "--tool",
            "claude",
            "--subagent",
            event,
        ];
        let crate::cli::command::Command::StopHook(args) =
            <crate::cli::command::Cli as clap::Parser>::try_parse_from(argv)
                .unwrap()
                .command
        else {
            panic!("stop-hook");
        };
        args
    };
    let stop = recorded("subagent_stop_a_parks.json", &repo, "sess-child").to_string();
    let start = documented_start(&repo, "sess-child", RECORDED_A).to_string();
    let recorded_frame = || Frame::bare(WireVerdict::Recorded);
    for (event, stdin, answers, within, stderr) in [
        ("stop", &stop, vec![recorded_frame()], 0, ""),
        ("start", &start, vec![recorded_frame()], 0, ""),
        ("stop", &stop, vec![], SUBAGENT_BUDGET.as_secs(), ""),
        ("stop", &stop, vec![Frame::bare(WireVerdict::Park)], 0, ""),
        (
            "start",
            &start,
            vec![Frame::bare(WireVerdict::Diagnostic {
                message: "hook: clank still has no monitor".into(),
            })],
            0,
            "hook: clank still has no monitor\n",
        ),
    ] {
        let mut host = Fed::new(stdin.clone(), answers);
        let began = tokio::time::Instant::now();
        run(args(event), &mut host).await.unwrap();
        let took = began.elapsed();
        assert!(
            took <= std::time::Duration::from_secs(within + 1),
            "{event}: held for {took:?}"
        );
        assert_eq!(host.exit, Some(HOOK_OK_EXIT), "{event}");
        assert!(host.stdout.is_empty(), "{event}: {:?}", host.stdout);
        assert_eq!(String::from_utf8_lossy(&host.stderr), stderr, "{event}");
        let carried = host.carried.borrow();
        assert!(
            matches!(
                carried.as_slice(),
                [Report::Child { seen, .. }] if seen.hook_started == Some(HOOK_STARTED)
            ),
            "{event}: {carried:?}"
        );
    }
}

/// A once-answered report's presentation is answered by how its writing
/// went: delivered only by a write that succeeded, failed by one that
/// failed or never happened, and a frame with no presentation is answered
/// with no result.
#[test]
fn a_once_answered_presentation_is_delivered_only_by_its_write() {
    let id = clank_core::turn_policy::PresentationId {
        session: clank_core::SessionId::parse("session-one").unwrap(),
        seq: 1,
    };
    let frame = Frame::text(
        WireVerdict::Deliver {
            presentation: id.clone(),
        },
        "clank check-in: x".into(),
    );
    assert_eq!(
        result_of(&frame, Some(Ok(())), "unwritten"),
        Some(TurnResult::Delivered {
            presentation: id.clone()
        })
    );
    assert_eq!(
        result_of(
            &frame,
            Some(Err(std::io::Error::other("closed"))),
            "unwritten"
        ),
        Some(TurnResult::EmitFailed {
            presentation: id.clone(),
            why: "closed".into()
        })
    );
    assert_eq!(
        result_of(&frame, None, "unwritten"),
        Some(TurnResult::EmitFailed {
            presentation: id,
            why: "unwritten".into()
        })
    );
    let notice = Frame::text(WireVerdict::Notify, "arm".into());
    assert_eq!(result_of(&notice, Some(Ok(())), "unwritten"), None);
}
