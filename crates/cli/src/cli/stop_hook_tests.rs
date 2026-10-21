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
        verdict_deadline(started) < started + ceiling,
        "the hook stops waiting BEFORE the runner kills it"
    );
    assert_eq!(
        (started + ceiling) - poll_deadline(started),
        std::time::Duration::from_secs(POLL_MARGIN_SECS),
        "the report's deadline leaves exactly the margin"
    );
    assert_eq!(
        verdict_deadline(started) - poll_deadline(started),
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
    let got = await_verdict_under_deadline(deadline, Tool::Codex, true, &mut life, || {
        let _ = end.send(());
        Ok(wait)
    })
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
    let got =
        await_verdict_under_deadline(deadline, Tool::Codex, true, &mut Lifetime::Gone, || {
            armed = true;
            Ok(pending_wait())
        })
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
        await_verdict_under_deadline(deadline, Tool::Codex, true, &mut life, || {
            Ok(plain(async move {
                let _guard = guard;
                std::future::pending::<Result<Frame, String>>().await
            }))
        }),
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
    let got = await_verdict_under_deadline(deadline, Tool::Codex, true, &mut life, || {
        Ok(plain(async move {
            let _ = end.send(());
            Ok(delivered())
        }))
    })
    .await;
    assert!(owner_gone(&got));
}

#[tokio::test]
async fn a_live_owner_lets_the_verdict_through() {
    let (mut life, _end) = Lifetime::signalled();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let got = await_verdict_under_deadline(deadline, Tool::Codex, true, &mut life, || {
        Ok(plain(async { Ok(delivered()) }))
    })
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
        let got =
            await_verdict_under_deadline(deadline, tool, true, &mut Lifetime::Unbound, || {
                Ok(pending_wait())
            })
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
    let got =
        await_verdict_under_deadline(past, Tool::Claude, true, &mut Lifetime::Unbound, || {
            armed.set(true);
            Ok(pending_wait())
        })
        .await;
    assert!(
        matches!(got, Err(ref f) if matches!(f.outcome(), HookOutcome::Continue { .. })),
        "the pre-check must re-arm too"
    );
    assert!(!armed.get(), "it must short-circuit BEFORE arming");
    let got =
        await_verdict_under_deadline(past, Tool::OpenCode, true, &mut Lifetime::Unbound, || {
            Ok(pending_wait())
        })
        .await;
    assert!(matches!(got, Err(ref f) if f.outcome() == HookOutcome::RenewWait));
}

#[tokio::test(start_paused = true)]
async fn a_verdict_arriving_before_the_deadline_survives_the_seam() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let got =
        await_verdict_under_deadline(deadline, Tool::Claude, true, &mut Lifetime::Unbound, || {
            Ok(plain(async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(delivered())
            }))
        })
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
        || Ok(pending_wait()),
    )
    .await;
    assert!(matches!(got, Err(ref f) if f.verdict == WireVerdict::Park));
}

fn stop_report() -> Report {
    Report::TurnEnd {
        stop: TurnEndReport {
            tool: Tool::Claude,
            session: clank_core::SessionId::parse("sess-handshake").unwrap(),
            place: Default::default(),
            tasks: TaskSnapshot::Known(Vec::new()),
        },
        deadline: u64::MAX,
        questions: Vec::new(),
    }
}

/// The owner handle is held for the whole wait: anything that closed the
/// child's stdin early would give every carried report instant EOF.
#[tokio::test]
async fn the_owner_handle_is_held_for_the_whole_wait() {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("cat >/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut owned = OwnedWait::spawn(&mut cmd, &stop_report()).expect("sh spawns with a pipe");
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
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!(
            "cat >/dev/null; sleep 0.2; touch '{}'",
            marker.display()
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (mut life, end) = Lifetime::signalled();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = end.send(());
    });
    let got = await_verdict_under_deadline(deadline, Tool::Codex, true, &mut life, || {
        OwnedWait::spawn(&mut cmd, &stop_report())
    })
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
fn child(frames: &[Frame], out: &Path) -> Command {
    let lines: Vec<String> = frames
        .iter()
        .map(|f| serde_json::to_string(f).unwrap())
        .collect();
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!(
            "read report; printf '%s\\n' \"$FRAMES\"; if read result; then printf '%s' \"$result\" \
             > '{0}'; else printf eof > '{0}'; fi",
            out.display()
        ))
        .env("FRAMES", lines.join("\n"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// The hook side of the handshake: a verdict is read while the child is
/// still alive and waiting, a parked verdict is not terminal, and the one
/// result goes back on the same pipe once the flush is known.
#[tokio::test]
async fn a_verdict_is_read_from_a_live_child_and_answered_on_its_pipe() {
    for (flush_fails, want) in [(false, "delivered"), (true, "emit_failed")] {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("result");
        let mut cmd = child(&[Frame::bare(WireVerdict::Park), delivered()], &out);
        let mut wait = OwnedWait::spawn(&mut cmd, &stop_report()).unwrap();
        let Ran::Verdict(Ok(frame)) = wait.verdict_until(std::future::pending()).await else {
            panic!("a verdict");
        };
        assert_eq!(frame, delivered(), "the parked frame is not terminal");
        assert!(
            wait.child.try_wait().unwrap().is_none(),
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
    let mut cmd = child(&[delivered()], &out);
    let mut wait = OwnedWait::spawn(&mut cmd, &stop_report()).unwrap();
    let Ran::Verdict(Ok(_)) = wait.verdict_until(std::future::pending()).await else {
        panic!("a verdict");
    };
    let OwnedWait {
        mut child,
        owner_end,
        ..
    } = wait;
    drop(owner_end);
    tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("the child exits on EOF")
        .unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "eof");
}

/// A child that ends without a terminal verdict is a stated failure that
/// carries what it said.
#[tokio::test]
async fn a_child_that_ends_without_a_verdict_is_diagnostic() {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("read report; echo 'monitor refused' >&2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut wait = OwnedWait::spawn(&mut cmd, &stop_report()).unwrap();
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
            crate::cli::wake_render::MONITOR_UNAVAILABLE.into(),
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

/// The Stop's evidence is what the report carries: the provider's own
/// listing when it sent one, however empty; `Unavailable` when it sent
/// none; and the deadline the hook can wait to.
#[tokio::test]
async fn the_report_carries_the_stops_evidence() {
    let dir = init_repo();
    let repo = dunce::canonicalize(dir.path()).unwrap();
    bind_tool(&repo, "claude", "sess-evidence", Tool::Claude);
    let mut input = hook_input("sess-evidence");
    input.background_tasks = Some(vec![
        serde_json::from_value(serde_json::json!({
            "id": "a1", "type": "subagent", "status": "running", "description": "sleeper",
        }))
        .unwrap(),
    ]);
    let recorder = Recorder::default();
    let before = super::super::turn_custody::now_ms();
    let turn = stop(
        Tool::Claude,
        Some(&repo),
        input,
        std::time::Instant::now(),
        &stated(),
        &recorder,
    )
    .await;
    assert!(
        matches!(turn.outcome, HookOutcome::Diagnostic { ref message } if message == "recorded")
    );
    {
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
        assert!(
            matches!(&reported.tasks, TaskSnapshot::Known(listed) if listed[0].raw["description"] == "sleeper")
        );
        assert!(*deadline > before, "the deadline is ahead of the Stop");
    }

    bind_tool(&repo, "codex", "sess-unlisted", Tool::Codex);
    stop(
        Tool::Codex,
        Some(&repo),
        hook_input("sess-unlisted"),
        std::time::Instant::now(),
        &stated(),
        &recorder,
    )
    .await;
    let carried = recorder.0.borrow();
    let (_, _, Report::TurnEnd { stop: reported, .. }) = &carried[1] else {
        panic!("a Stop's report");
    };
    assert_eq!(
        reported.tasks,
        TaskSnapshot::Unavailable,
        "codex sends no listing"
    );
}

#[tokio::test]
async fn unresolved_identity_is_a_diagnostic() {
    let dir = init_repo();
    let turn = stop(
        Tool::Claude,
        Some(dir.path()),
        hook_input("sess-unbound"),
        std::time::Instant::now(),
        &stated(),
        &Unreachable,
    )
    .await;
    assert!(matches!(turn.outcome, HookOutcome::Diagnostic { .. }));
}

/// The seam carries what a test SAYS: a stated `CLANK_AGENT` outranks the
/// session binding, and a label this session does not hold cannot be
/// reported for. With nothing stated, the binding decides, and the Stop
/// is carried for that label.
#[tokio::test]
async fn a_stated_override_cannot_report_for_an_unbound_label() {
    let dir = init_repo();
    bind_tool(dir.path(), "codex", "sess-stated-override", Tool::Claude);
    let elsewhere = crate::agent_env::Stated::saying(&[("CLANK_AGENT", "somebody-else")]);
    let turn = stop(
        Tool::Claude,
        Some(dir.path()),
        hook_input("sess-stated-override"),
        std::time::Instant::now(),
        &elsewhere,
        &Unreachable,
    )
    .await;
    match turn.outcome {
        HookOutcome::Diagnostic { message } => {
            assert!(message.contains("somebody-else"), "{message}")
        }
        HookOutcome::Silent {
            why: SilentReason::StaleBinding,
        } => {}
        other => panic!("a label that is not bound cannot report: {other:?}"),
    }
    let recorder = Recorder::default();
    stop(
        Tool::Claude,
        Some(dir.path()),
        hook_input("sess-stated-override"),
        std::time::Instant::now(),
        &stated(),
        &recorder,
    )
    .await;
    assert_eq!(recorder.0.borrow()[0].1, "codex", "the binding decides");
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
        input.clone(),
        std::time::Instant::now(),
        &stated(),
        &recorder,
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
    let boundary = native_boundary(Tool::Claude, &input);
    assert_eq!(
        question_evidence(Tool::Claude, &input, boundary.as_ref()),
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
    assert!(question_evidence(Tool::Codex, &input, boundary.as_ref()).is_empty());
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

/// A label that another agent held is cleared off it by the bind: one
/// session, one label.
#[test]
fn a_launched_session_takes_its_id_off_any_other_label() {
    let dir = init_repo();
    let repo = dir.path();
    let sid = "01a08361-f112-75d0-a49f-2e35b104e3b6";
    bind_tool(repo, "old", sid, Tool::Codex);
    let new = AgentLabel::parse("new").unwrap();
    roster_agent(repo, &new, Tool::Codex);
    session_started(
        repo,
        Tool::Codex,
        &start_input(sid, "startup"),
        Some(&new),
        &stated(),
    );
    assert_eq!(
        bound_session(repo, "new"),
        Some((Tool::Codex, sid.to_string()))
    );
    assert_eq!(bound_session(repo, "old"), None);
}

#[test]
fn claude_compatible_grok_start_binds_the_roster_tool() {
    let dir = init_repo();
    let repo = dir.path();
    let label = AgentLabel::parse("grok").unwrap();
    roster_agent(repo, &label, Tool::Grok);
    let sid = "01a0dd00-5b0f-7dc0-b418-24054b5112cf";
    let mut input = start_input(sid, "resume");
    input.transcript_path = Some("/tmp/.grok/updates.jsonl".into());
    let got = session_started(repo, Tool::Claude, &input, Some(&label), &stated());
    assert_eq!(got, Some((label.clone(), Tool::Grok)));
    assert_eq!(bound_session(repo, "grok"), Some((Tool::Grok, sid.into())));
    assert!(crate::agent_store::binding_matches(
        repo,
        &label,
        Tool::Grok,
        sid
    ));
}

/// No label in the environment: a session clank did not launch. It is
/// identified by its id as `clank as` bound it, and an unbound id binds
/// nothing.
#[test]
fn a_hand_started_session_is_identified_by_its_id_and_never_bound_here() {
    let dir = init_repo();
    let repo = dir.path();
    let sid = "9c96eb03-1458-4bac-abbb-90243cfd422c";
    assert!(
        session_started(
            repo,
            Tool::Claude,
            &start_input(sid, "startup"),
            None,
            &stated()
        )
        .is_none()
    );
    assert!(
        crate::agent_store::load_all_agent_configs(repo)
            .unwrap()
            .is_empty()
    );
    bind_tool(repo, "claude", sid, Tool::Claude);
    let got = session_started(
        repo,
        Tool::Claude,
        &start_input(sid, "clear"),
        None,
        &stated(),
    );
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
            crate::cli::wake_render::MONITOR_UNAVAILABLE.into(),
        ),
        &binding,
        answered.clone(),
        &mut writer,
    )
    .await;
    assert!(String::from_utf8_lossy(&writer.bytes).contains("monitor is unavailable"));
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
