//! `clank stop-hook` — the hook adapters `clank setup` installs into
//! `~/.claude/settings.json` and `~/.codex/hooks.json`, and that
//! OpenCode's plugin runs.
//!
//! The hook is a messenger. The monitor decides everything about an
//! agent's turn-ends ([`super::turn_custody`]); the hook carries the
//! evidence in and the verdict out. Grok's native lifecycle dispatches to
//! [`super::grok_hook`] first, and recovery's own modes to
//! [`super::recovery_hook`].
//!
//! # A Stop turn
//!
//! 1. **Route.** Repo from `--repo`, else the stdin `cwd`; label from the
//!    environment, else the binding. If the binding no longer names this
//!    session and this tool, be Silent: someone else is this label now.
//! 2. **Report.** The Stop's evidence — its place in the agent's record,
//!    what it proves about the agent's questions, and the background work
//!    the provider listed, or that it listed none — goes to the monitor in
//!    the hello of a `clank wait --turn-end` child.
//! 3. **Carry.** Each verdict arrives as a line on the child's stdout
//!    while the child and its connection stay alive. `Park` holds the
//!    wait; the first terminal verdict ends it.
//! 4. **Frame** the verdict in the tool's wire shape, re-checking the
//!    binding first: a park can outlive the incarnation that armed it.
//! 5. **Answer** a presentation with exactly one result line on the
//!    child's stdin — delivered, or the emit failed — then close it.
//!
//! The child's stdin is also the ownership sentinel: if this process is
//! killed, SIGKILL included, the pipe closes and the child exits, and the
//! monitor reads the connection closing without a result as a failed
//! emit. Nothing here reads or writes an agent's state.
//!
//! # A SessionStart (`--session-start`)
//!
//! Bind the session clank launched to its label, or identify one it did
//! not launch by its id; then report the start, and surface any work the
//! monitor presents on `additionalContext`. Fail-open at every step: a
//! session start is never broken by clank state.
//!
//! # The exit contracts
//!
//! **A Stop turn.** Exit 2 is Claude's continuation channel and nothing
//! else. Every other outcome — Silent, Diagnostic, Codex's block JSON,
//! OpenCode's renew wire — exits [`HOOK_OK_EXIT`], and no Stop error path
//! exits non-zero: each becomes a [`HookOutcome::Diagnostic`].
//!
//! **A SessionStart** produces no [`HookOutcome`]: every path exits 0.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::turn_wire::{Frame, Report, TurnResult, WireVerdict};
use super::{StopHookArgs, resolve_cwd_repo, resolve_repo};
use crate::agent_env::{Ambient, Process, resolve_identity_for_hook_in};
use crate::lifecycle::AgentLabel;
use clank_core::turn_policy::{PresentationId, TaskSnapshot, TurnEndReport, TurnPlace};
use clank_core::{
    CLAUDE_CONTINUATION_EXIT, CodexBlockDecision, HOOK_OK_EXIT, HookInput, HookOutcome,
    SilentReason, Tool,
};

pub async fn run(args: StopHookArgs) -> anyhow::Result<()> {
    // Grok imports Claude hook files too. Suppress only Clank's imported
    // adapter; the native adapter below owns this lifecycle. Other user hooks
    // are unaffected. Never park Grok inside the Claude hook protocol.
    if std::env::var_os("GROK_HOOK_EVENT").is_some() && Tool::from(args.tool) != Tool::Grok {
        return Ok(());
    }
    if Tool::from(args.tool) == Tool::Grok {
        return super::grok_hook::hook(args).await;
    }
    if args.observe.is_some() || args.recovery_helper || args.recovery_reply.is_some() {
        return super::recovery_hook::run(args).await;
    }
    if args.session_start {
        return run_session_start(args).await;
    }
    // The runner's ceiling clock starts HERE, at process spawn — so the
    // wait's deadline must be measured from here too.
    let started = std::time::Instant::now();
    let tool: Tool = args.tool.into();
    let turn = match read_hook_stdin() {
        Ok(input) => {
            stop(
                tool,
                args.repo.as_deref(),
                input,
                started,
                &Process,
                &Spawner,
            )
            .await
        }
        Err(e) => Turn::said(HookOutcome::Diagnostic { message: e }),
    };
    let code = emit_turn(turn, tool, &mut std::io::stdout(), &mut std::io::stderr()).await;
    crate::cli::source_group::exit(code)
}

/// One Stop's answer: the outcome to frame, and the wait that carried it,
/// which a presentation must be answered on.
struct Turn {
    outcome: HookOutcome,
    presentation: Option<PresentationId>,
    binding: Option<Binding>,
    wait: Option<Box<dyn Answer>>,
}

impl Turn {
    fn said(outcome: HookOutcome) -> Self {
        Turn {
            outcome,
            presentation: None,
            binding: None,
            wait: None,
        }
    }
}

/// Who this hook speaks for, re-checked before anything is emitted.
struct Binding {
    repo: PathBuf,
    label: AgentLabel,
    tool: Tool,
    session: String,
}

impl Binding {
    fn current(&self) -> bool {
        crate::agent_store::binding_matches(&self.repo, &self.label, self.tool, &self.session)
    }
}

/// The Stop, from routing to the verdict.
async fn stop(
    tool: Tool,
    repo_override: Option<&Path>,
    input: HookInput,
    started: std::time::Instant,
    env: &dyn Ambient,
    spawner: &dyn Spawn,
) -> Turn {
    // Read now, before any waiting: where in the agent's record this Stop
    // came from, not wherever the record has got to by delivery.
    let boundary = native_boundary(tool, &input);
    let questions = question_evidence(tool, &input, boundary.as_ref());
    let repo = match resolve_hook_repo(repo_override, &input) {
        Ok(r) => r,
        Err(message) => return Turn::said(HookOutcome::Diagnostic { message }),
    };
    let label = match resolve_identity_for_hook_in(&repo, tool, &input.session_id, env) {
        Ok(l) => l,
        Err(e) => {
            return Turn::said(HookOutcome::Diagnostic {
                message: format!("{e:#}"),
            });
        }
    };
    let binding = Binding {
        repo,
        label,
        tool,
        session: input.session_id.as_str().to_string(),
    };
    if !binding.current() {
        return Turn::said(HookOutcome::Silent {
            why: SilentReason::StaleBinding,
        });
    }
    let report = Report::TurnEnd {
        stop: TurnEndReport {
            tool,
            session: input.session_id.clone(),
            place: boundary
                .map(|b| TurnPlace {
                    turn: Some(b.turn),
                    position: Some(b.position),
                    human_turn: b.human,
                })
                .unwrap_or_default(),
            tasks: TaskSnapshot::of(&input),
        },
        deadline: unix_ms(poll_deadline(started)),
        questions,
    };
    let mut life = super::hook_owner::lifetime_for(tool, &super::hook_owner::Native);
    let carried =
        await_verdict_under_deadline(verdict_deadline(started), tool, true, &mut life, || {
            spawner.spawn(
                &binding.repo,
                &binding.label,
                &binding.session,
                tool,
                &report,
            )
        })
        .await;
    match carried {
        Ok((frame, wait)) => Turn {
            outcome: frame.outcome(),
            presentation: frame.presentation().cloned(),
            binding: Some(binding),
            wait: Some(Box::new(wait)),
        },
        Err(frame) => Turn {
            outcome: frame.outcome(),
            presentation: None,
            binding: Some(binding),
            wait: None,
        },
    }
}

fn unix_ms(at: std::time::Instant) -> u64 {
    let ahead = at.saturating_duration_since(std::time::Instant::now());
    super::turn_custody::now_ms() + ahead.as_millis() as u64
}

/// What the Stop proves about the agent's questions: the turn it ends,
/// and the prompts its record shows behind that turn. Only Claude asks
/// them.
fn question_evidence(
    tool: Tool,
    input: &HookInput,
    boundary: Option<&super::wake_repeat::Boundary>,
) -> Vec<super::question_state::Observation> {
    use super::question_state::Observation;
    let (Tool::Claude, Some(b), Some(transcript)) = (tool, boundary, &input.transcript_path) else {
        return Vec::new();
    };
    let session = input.session_id.as_str().to_string();
    vec![
        Observation::TurnOver {
            session: session.clone(),
            prompt: b.turn.clone(),
        },
        Observation::Behind {
            session,
            older: super::question_state::prompts_behind(Path::new(transcript), &b.turn),
        },
    ]
}

/// Where this Stop comes from in the agent's own record, read when the
/// hook is invoked, before any waiting.
fn native_boundary(tool: Tool, input: &HookInput) -> Option<super::wake_repeat::Boundary> {
    let record = Path::new(input.transcript_path.as_deref()?);
    match tool {
        Tool::Claude => super::wake_repeat::claude_boundary(record),
        Tool::Codex => super::wake_repeat::codex_boundary(record, input.turn_id.as_deref()?),
        _ => None,
    }
}

/// The SessionStart payload both tools send: claude documents it, codex
/// 0.153 carries the same fields.
#[derive(serde::Deserialize)]
struct SessionStartInput {
    session_id: String,
    cwd: String,
    /// `startup` / `resume` / `clear` / `compact`, for the diagnostic line.
    #[serde(default)]
    source: Option<String>,
    /// Claude sends the session's own transcript file; codex does not.
    #[serde(default)]
    transcript_path: Option<String>,
}

/// Who this session is, and — when clank launched it — the binding that
/// says so.
///
/// A session clank launched carries `CLANK_AGENT`, and that label is bound
/// to the id on stdin: on `startup` the first bind, on `clear` the rebind
/// that keeps a session tracked across a new id, on `resume`/`compact` an
/// idempotent re-assertion. A session clank did not launch is identified
/// by its id, as `clank as` bound it.
fn session_started(
    repo: &Path,
    tool: Tool,
    input: &SessionStartInput,
    launched_as: Option<&AgentLabel>,
    env: &dyn Ambient,
) -> Option<(AgentLabel, Tool)> {
    let sid = clank_core::ids::SessionId::parse(&input.session_id).ok()?;
    match launched_as {
        Some(label) => {
            let binding_tool = match crate::agent_store::resolve_tool(repo, label) {
                Ok(tool) => tool,
                Err(e) => {
                    eprintln!(
                        "clank: cannot resolve roster tool for `{label}` at session start: {e:#}"
                    );
                    return None;
                }
            };
            // Fail-open by POLICY: a bind that fails leaves the previous
            // binding, and the Stop hook still resolves this session by its
            // label; the diagnostic names what did not happen.
            if let Err(e) =
                crate::agent_store::bind_session_to_agent(repo, label, binding_tool, &sid)
            {
                eprintln!(
                    "clank: could not bind `{}` to {} session {} ({}): {e:#}",
                    label.as_str(),
                    binding_tool.as_str(),
                    sid.as_str(),
                    input.source.as_deref().unwrap_or("start"),
                );
            } else if let Some(path) = input.transcript_path.as_deref() {
                let _ = crate::agent_store::record_transcript(repo, label, &sid, path);
            }
            Some((label.clone(), binding_tool))
        }
        None => {
            let label = resolve_identity_for_hook_in(repo, tool, &sid, env).ok()?;
            Some((label, tool))
        }
    }
}

/// How long a session start may take to hear what it is owed: well under
/// the 15s its hook entry allows.
const SESSION_START_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// SessionStart: bind, report the start, and surface on
/// `additionalContext` what the monitor presents. Every path exits 0.
async fn run_session_start(args: StopHookArgs) -> anyhow::Result<()> {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return Ok(());
    }
    let Ok(input) = serde_json::from_str::<SessionStartInput>(&raw) else {
        return Ok(());
    };
    let Ok(repo) = session_start_repo(args.repo.as_deref(), &input.cwd) else {
        return Ok(());
    };
    let tool: Tool = args.tool.into();
    let launched_as = crate::agent_env::explicit_label_in(&Process).ok().flatten();
    let Some((label, binding_tool)) =
        session_started(&repo, tool, &input, launched_as.as_ref(), &Process)
    else {
        return Ok(());
    };
    let Ok(session) = clank_core::SessionId::parse(&input.session_id) else {
        return Ok(());
    };
    let started = std::time::Instant::now();
    let report = Report::SessionStart {
        tool: binding_tool,
        session: session.clone(),
        deadline: unix_ms(started + SESSION_START_BUDGET),
    };
    let binding = Binding {
        repo,
        label,
        tool: binding_tool,
        session: input.session_id,
    };
    let carried = await_verdict_under_deadline(
        started + SESSION_START_BUDGET,
        binding_tool,
        false,
        &mut super::hook_owner::Lifetime::Unbound,
        || {
            Spawner.spawn(
                &binding.repo,
                &binding.label,
                &binding.session,
                binding_tool,
                &report,
            )
        },
    )
    .await;
    if let Ok((frame, wait)) = carried {
        emit_session_start(frame, &binding, wait, &mut std::io::stdout()).await;
    }
    Ok(())
}

/// Carry `report`, which never parks, to the monitor and return its
/// verdict, or the one its tool takes when none comes within `budget`.
pub(super) async fn answer_once(
    repo: &Path,
    label: &AgentLabel,
    session: &str,
    report: Report,
    budget: std::time::Duration,
) -> Frame {
    let tool = report.tool();
    let carried = await_verdict_under_deadline(
        std::time::Instant::now() + budget,
        tool,
        false,
        &mut super::hook_owner::Lifetime::Unbound,
        || Spawner.spawn(repo, label, session, tool, &report),
    )
    .await;
    match carried {
        Ok((frame, wait)) => {
            Box::new(wait).answer(None).await;
            frame
        }
        Err(frame) => frame,
    }
}

/// Surface a session start's verdict: work or a notice as
/// `additionalContext`, nothing otherwise, and a presentation answered
/// once its flush is known.
async fn emit_session_start(
    frame: Frame,
    binding: &Binding,
    wait: impl Answer + 'static,
    stdout: &mut impl std::io::Write,
) {
    let (Some(text), true) = (
        frame.rendered.as_deref(),
        matches!(
            frame.verdict,
            WireVerdict::Deliver { .. } | WireVerdict::Notify
        ),
    ) else {
        Box::new(wait).answer(None).await;
        return;
    };
    let emitted = binding.current().then(|| {
        let payload = session_start_payload(text);
        serde_json::to_writer(&mut *stdout, &payload)?;
        stdout.write_all(b"\n")?;
        stdout.flush()
    });
    let result = frame.presentation().map(|id| match emitted {
        Some(Ok(())) => TurnResult::Delivered {
            presentation: id.clone(),
        },
        Some(Err(e)) => TurnResult::EmitFailed {
            presentation: id.clone(),
            why: e.to_string(),
        },
        None => TurnResult::EmitFailed {
            presentation: id.clone(),
            why: "the session no longer holds the label".into(),
        },
    });
    Box::new(wait).answer(result).await;
}

/// The SessionStart hook-output wire: pending work rides
/// `additionalContext`.
fn session_start_payload(items_text: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": format!(
                "Pending clank work found at session start.\n{items_text}"
            ),
        }
    })
}

use super::hook_owner::Lifetime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Argv for the child that carries a report. Separate from the spawn so
/// the shape can be asserted without launching anything. It carries no
/// role: the monitor derives that from `--author`'s roster entry.
fn wait_argv(repo: &Path, label: &AgentLabel, session: &str) -> Vec<String> {
    vec![
        "wait".into(),
        "--repo".into(),
        repo.display().to_string(),
        "--author".into(),
        label.as_str().to_string(),
        // Ownership, not decoration: without this the child has no bound
        // at all and outlives a killed hook forever.
        "--die-with-owner".into(),
        "--deliver-to-session".into(),
        session.into(),
        "--turn-end".into(),
    ]
}

fn wait_command(exe: &Path, repo: &Path, label: &AgentLabel, session: &str, tool: Tool) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(wait_argv(repo, label, session))
        // Piped stdin IS the ownership channel, and the report's way in.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in crate::agent_env::SESSION_IDENTITY_VARS {
        cmd.env_remove(key);
    }
    let key = match tool {
        Tool::Claude => "CLAUDE_CODE_SESSION_ID",
        Tool::Codex => "CODEX_THREAD_ID",
        Tool::OpenCode => "OPENCODE_SESSION_ID",
        Tool::Grok => "GROK_AGENT",
    };
    cmd.env(key, session);
    cmd
}

/// What starts the child that carries a report.
trait Spawn {
    fn spawn(
        &self,
        repo: &Path,
        label: &AgentLabel,
        session: &str,
        tool: Tool,
        report: &Report,
    ) -> Result<OwnedWait, String>;
}

/// This binary, as `clank wait --turn-end`.
struct Spawner;

impl Spawn for Spawner {
    fn spawn(
        &self,
        repo: &Path,
        label: &AgentLabel,
        session: &str,
        tool: Tool,
        report: &Report,
    ) -> Result<OwnedWait, String> {
        let exe = std::env::current_exe().map_err(|e| format!("hook: current_exe failed: {e}"))?;
        let mut cmd = wait_command(&exe, repo, label, session, tool);
        OwnedWait::spawn(&mut cmd, report)
    }
}

/// A spawned child bundled with the owner handle whose LIFETIME is the
/// ownership contract.
///
/// The handle is a required field, so a child with no owner cannot be
/// constructed, and it is released only by [`Answer::answer`] or
/// [`OwnedWait::shutdown`], after the result it carries is written.
/// Closing it early would read as instant owner death.
struct OwnedWait {
    child: tokio::process::Child,
    owner_end: tokio::process::ChildStdin,
    /// The child's verdicts, one per line, read while it stays alive.
    frames: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    stderr: tokio::task::JoinHandle<String>,
    /// Whether the report this child carries can park.
    parks: bool,
    /// The report's line, written before the first verdict is read.
    pending_report: Option<String>,
}

impl OwnedWait {
    /// Spawn `cmd` and hand it `report` on its first stdin line, requiring
    /// the ownership pipe: a child without one is a hard error rather than
    /// a silently unowned wait.
    fn spawn(cmd: &mut Command, report: &Report) -> Result<Self, String> {
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("hook: spawning wait failed: {e}"))?;
        let owner_end = child.stdin.take().ok_or_else(|| {
            "hook: wait spawned without a stdin pipe — the ownership sentinel would be absent, \
             so refusing to run it"
                .to_string()
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "hook: wait spawned without a stdout pipe".to_string())?;
        let mut stderr = child.stderr.take();
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            if let Some(stderr) = stderr.as_mut() {
                use tokio::io::AsyncReadExt;
                let _ = stderr.take(64 * 1024).read_to_string(&mut text).await;
            }
            text
        });
        let mut line = serde_json::to_string(report).map_err(|e| format!("hook: {e}"))?;
        line.push('\n');
        Ok(OwnedWait {
            child,
            owner_end,
            frames: tokio::io::BufReader::new(stdout).lines(),
            stderr,
            parks: report.parks(),
            pending_report: Some(line),
        })
    }
}

/// How long an abandoned child gets to finish its own shutdown before it
/// is killed.
const ABANDON_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// How a carried report ended before its verdict was framed.
enum Ran {
    Verdict(Result<Frame, String>),
    /// The owner went away first.
    Abandoned,
}

/// A running child that the hook's owner going away ends cooperatively.
trait Run {
    /// Read verdicts until a terminal one, or until `abandon` resolves.
    async fn verdict_until(&mut self, abandon: impl std::future::Future<Output = ()>) -> Ran;
    /// Close the owner pipe and let the child finish its own shutdown.
    async fn shutdown(self);
}

/// The one result a presentation is answered with, written on the pipe
/// that carried it, after which the pipe closes.
trait Answer {
    fn answer(
        self: Box<Self>,
        result: Option<TurnResult>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
}

impl Run for OwnedWait {
    async fn verdict_until(&mut self, abandon: impl std::future::Future<Output = ()>) -> Ran {
        tokio::pin!(abandon);
        if let Some(line) = self.pending_report.take()
            && let Err(e) = self.owner_end.write_all(line.as_bytes()).await
        {
            return Ran::Verdict(Err(format!("hook: handing the wait its report: {e}")));
        }
        loop {
            let line = tokio::select! {
                biased;
                () = &mut abandon => return Ran::Abandoned,
                line = self.frames.next_line() => line,
            };
            let frame = match line {
                Ok(Some(line)) => serde_json::from_str::<Frame>(&line)
                    .map_err(|e| format!("hook: wait stdout malformed: {e}")),
                Ok(None) | Err(_) => {
                    let said = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        &mut self.stderr,
                    )
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                    Err(format!(
                        "hook: the wait ended without a verdict: {}",
                        said.trim()
                    ))
                }
            };
            match frame {
                Ok(frame) if !frame.terminal(self.parks) => continue,
                other => return Ran::Verdict(other),
            }
        }
    }

    async fn shutdown(self) {
        let OwnedWait {
            mut child,
            owner_end,
            ..
        } = self;
        drop(owner_end);
        let _ = tokio::time::timeout(ABANDON_GRACE, child.wait()).await;
    }
}

impl Answer for OwnedWait {
    fn answer(
        mut self: Box<Self>,
        result: Option<TurnResult>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            if let Some(line) = result.and_then(|r| serde_json::to_string(&r).ok()) {
                let _ = self
                    .owner_end
                    .write_all(format!("{line}\n").as_bytes())
                    .await;
                let _ = self.owner_end.flush().await;
            }
            self.shutdown().await;
        })
    }
}

/// Margin between the report's deadline and the hook runner's ceiling.
///
/// The runner's clock starts at PROCESS SPAWN, so the margin covers
/// everything else this process does. Generous on purpose: expiring early
/// costs one re-park; expiring late is the runner killing the hook.
const POLL_MARGIN_SECS: u64 = 300;

/// How long past the report's deadline the hook waits for the verdict the
/// deadline itself produces, before it frames that verdict on its own.
const VERDICT_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// The ABSOLUTE instant the report names as its deadline, derived from the
/// ceiling clank itself writes into the tool's hook config. Absolute, so
/// time spent setting up is charged against it.
fn poll_deadline(started: std::time::Instant) -> std::time::Instant {
    let ceiling = std::time::Duration::from_secs(crate::cli::setup::HOOK_TIMEOUT_SECS);
    let margin = std::time::Duration::from_secs(POLL_MARGIN_SECS);
    started + ceiling.saturating_sub(margin)
}

/// When the hook stops waiting for a verdict at all.
fn verdict_deadline(started: std::time::Instant) -> std::time::Instant {
    poll_deadline(started) + VERDICT_GRACE
}

/// Await `fut` until `deadline`. `None` = the deadline won, and `fut` was
/// dropped — which is what closes the owner pipe so the child exits.
async fn under_deadline<F: std::future::Future>(
    fut: F,
    deadline: std::time::Instant,
) -> Option<F::Output> {
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), fut)
        .await
        .ok()
}

/// Everything the deadline and the owner govern: the pre-check, the
/// arming, and the wait for a terminal verdict.
///
/// Owner loss outranks whatever the child found, and the deadline: there
/// is nobody left to deliver to. A deadline with no verdict frames the
/// one the tool takes at its deadline, so a slow monitor never puts the
/// agent to sleep.
async fn await_verdict_under_deadline<W: Run>(
    deadline: std::time::Instant,
    tool: Tool,
    parks: bool,
    life: &mut Lifetime,
    arm: impl FnOnce() -> Result<W, String>,
) -> Result<(Frame, W), Frame> {
    let owner_gone = || {
        Frame::bare(WireVerdict::Silent {
            why: SilentReason::OwnerGone,
        })
    };
    let at_deadline = || match parks {
        true => super::turn_custody::Turns::deadline_frame(tool),
        false => Frame::bare(WireVerdict::Park),
    };
    if life.gone_now() {
        return Err(owner_gone());
    }
    // The rarer exit, and the strictly worse one: it dies having done no
    // waiting at all. Short-circuits BEFORE arming.
    if std::time::Instant::now() >= deadline {
        return Err(at_deadline());
    }
    let mut wait = match arm() {
        Ok(w) => w,
        Err(message) => return Err(Frame::bare(WireVerdict::Diagnostic { message })),
    };
    let ran = under_deadline(wait.verdict_until(life.ended()), deadline).await;
    match ran {
        Some(Ran::Abandoned) => {
            wait.shutdown().await;
            Err(owner_gone())
        }
        _ if life.gone_now() => {
            wait.shutdown().await;
            Err(owner_gone())
        }
        None => {
            wait.shutdown().await;
            Err(at_deadline())
        }
        Some(Ran::Verdict(Ok(frame))) => Ok((frame, wait)),
        Some(Ran::Verdict(Err(message))) => {
            wait.shutdown().await;
            Err(Frame::bare(WireVerdict::Diagnostic { message }))
        }
    }
}

pub(crate) fn read_hook_stdin() -> Result<HookInput, String> {
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        return Err(format!("hook: reading stdin: {e}"));
    }
    serde_json::from_str(&buf).map_err(|e| format!("hook: parsing stdin JSON: {e}"))
}

/// The repo a SessionStart binds in: `--repo` exactly, else the worktree
/// holding the session's cwd.
fn session_start_repo(repo_override: Option<&Path>, cwd: &str) -> anyhow::Result<PathBuf> {
    match repo_override {
        Some(p) => resolve_repo(Some(p)),
        None => resolve_cwd_repo(Path::new(cwd)),
    }
}

pub(crate) fn resolve_hook_repo(
    repo_override: Option<&Path>,
    input: &HookInput,
) -> Result<PathBuf, String> {
    if let Some(p) = repo_override {
        return resolve_repo(Some(p)).map_err(|e| format!("hook: --repo {p:?} invalid: {e:#}"));
    }
    // The agent is sitting at this directory when it ends a turn — that IS
    // the repo it works in.
    resolve_cwd_repo(Path::new(&input.cwd)).map_err(|e| {
        format!(
            "hook: cwd `{}` from hook stdin is not inside a git repo: {e:#}",
            input.cwd
        )
    })
}

/// Writes and flushes the established wire. `presented` is true only for
/// a continuation that reached the agent's channel; diagnostics and plugin
/// control present nothing.
fn emit_wire(
    outcome: &HookOutcome,
    tool: Tool,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
) -> std::io::Result<(i32, bool)> {
    use std::io::Write;
    let (stream, text, code, presented): (&mut dyn Write, std::borrow::Cow<'_, str>, i32, bool) =
        match (outcome, tool) {
            (HookOutcome::RenewWait, Tool::OpenCode) => {
                (stdout, OPENCODE_RENEW_WAIT_WIRE.into(), HOOK_OK_EXIT, false)
            }
            (HookOutcome::RenewWait, _) => (
                stderr,
                "hook: renew-wait action reached an unsupported adapter".into(),
                HOOK_OK_EXIT,
                false,
            ),
            (HookOutcome::Continue { reason }, Tool::Claude) => (
                stderr,
                reason.as_str().into(),
                CLAUDE_CONTINUATION_EXIT,
                true,
            ),
            (HookOutcome::Continue { reason }, Tool::Codex) => (
                stdout,
                serde_json::to_string(&CodexBlockDecision::from_reason(reason))?.into(),
                HOOK_OK_EXIT,
                true,
            ),
            (HookOutcome::Continue { reason }, Tool::OpenCode) => {
                (stdout, reason.as_str().into(), HOOK_OK_EXIT, true)
            }
            (HookOutcome::Continue { reason }, Tool::Grok) => {
                (stderr, reason.as_str().into(), HOOK_OK_EXIT, false)
            }
            (HookOutcome::Silent { .. }, _) => return Ok((HOOK_OK_EXIT, false)),
            (HookOutcome::Diagnostic { message }, _) => {
                (stderr, message.as_str().into(), HOOK_OK_EXIT, false)
            }
        };
    stream.write_all(text.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok((code, presented))
}

/// Frame a Stop's outcome, answer its presentation, and give the exit code.
/// A binding replaced while the hook waited emits nothing, and a
/// presentation counts as delivered only once its flush succeeded.
async fn emit_turn(
    turn: Turn,
    tool: Tool,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
) -> i32 {
    let Turn {
        mut outcome,
        presentation,
        binding,
        wait,
    } = turn;
    let stale = binding.as_ref().is_some_and(|b| !b.current());
    if stale {
        outcome = HookOutcome::Silent {
            why: SilentReason::StaleBinding,
        };
    }
    let emission = emit_wire(&outcome, tool, stdout, stderr);
    let fallback = match (&outcome, tool) {
        (HookOutcome::Continue { .. }, Tool::Claude) => CLAUDE_CONTINUATION_EXIT,
        _ => HOOK_OK_EXIT,
    };
    let result = presentation.map(|id| match &emission {
        Ok((_, true)) => TurnResult::Delivered { presentation: id },
        Ok((_, false)) if stale => TurnResult::EmitFailed {
            presentation: id,
            why: "the session no longer holds the label".into(),
        },
        Ok((_, false)) => TurnResult::EmitFailed {
            presentation: id,
            why: "nothing reached the agent".into(),
        },
        Err(e) => TurnResult::EmitFailed {
            presentation: id,
            why: e.to_string(),
        },
    });
    if let Some(wait) = wait {
        wait.answer(result).await;
    }
    match emission {
        Ok((code, _)) => code,
        Err(_) => fallback,
    }
}

/// Plugin-only control wire. It is intercepted before OpenCode's
/// non-empty stdout is treated as a model prompt.
const OPENCODE_RENEW_WAIT_WIRE: &str = r#"{"clank":"renew_wait"}"#;

#[cfg(test)]
#[path = "stop_hook_tests.rs"]
mod tests;
