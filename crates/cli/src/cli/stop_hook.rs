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
//!    what it proves about the agent's questions, and the harness's task
//!    listing or that it sent none — goes to the monitor in the hello of a
//!    `clank wait --turn-end` child.
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
//! # An install under a parked Stop (`--handoff`)
//!
//! A hook parked in step 3 looks at its installation on a beat. When a
//! different build is installed it becomes that build in place: `exec`
//! closes the child's pipes, which ends the child as a killed hook would,
//! and the new image starts a child of its own, so the frames between a
//! hook and its child are always one build's. What only the old image
//! held crosses in a [`super::hook_handoff::Handoff`]. An `exec` that
//! fails changes nothing: the park, its child and its report stand, and
//! it is tried again.
//!
//! # A PostCompact (`--post-compact`)
//!
//! A compaction can end the parked Stop wait, and runs no Stop after
//! it: an idle compaction on 2.1.308 left the agent with none. The same turn is carried again
//! as a [`Report::Compacted`], under the same contracts; the monitor's
//! newest wait for a session is its park, so this one, an older one and
//! a later Stop's converge on one.
//!
//! # A SubagentStop or SubagentStart (`--subagent`)
//!
//! Claude runs these for a subagent of the session, not for the agent. The
//! hook reports what it saw, hears that it was recorded and exits 0 having
//! written nothing: it never parks, and what it reports ends no turn.
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

use super::turn_wire::{Frame, Report, TurnResult, WireVerdict};
use super::{StopHookArgs, resolve_cwd_repo, resolve_repo};
use crate::agent_env::{Ambient, Process, resolve_identity_for_hook_in};
use crate::lifecycle::AgentLabel;
use clank_core::turn_policy::{
    ChildReport, ChildSeen, Ms, PresentationId, TaskSnapshot, TurnEndReport, TurnPlace,
};
use clank_core::{
    CLAUDE_CONTINUATION_EXIT, CodexBlockDecision, HOOK_OK_EXIT, HookInput, HookOutcome,
    SilentReason, Tool,
};

pub(crate) async fn run(args: StopHookArgs, host: &mut impl Host) -> anyhow::Result<()> {
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
    // Read before anything else this process does: the only evidence of
    // where this report sits among its session's others.
    let hook_started = host.now_ms();
    let tool: Tool = args.tool.into();
    if let Some(event) = args.subagent {
        run_subagent(tool, args.repo.as_deref(), event, hook_started, host).await;
        return Ok(());
    }
    let arrived = match args.handoff.as_deref() {
        Some(path) => super::hook_handoff::Handoff::take(path).and_then(Arrived::resumed),
        None => host
            .input()
            .and_then(|raw| Arrived::fresh(raw, started, hook_started)),
    };
    let turn = match arrived {
        Ok(arrived) => {
            let repo = args.repo.as_deref();
            stop(tool, repo, arrived, args.post_compact, &*host).await
        }
        Err(e) => Turn::said(HookOutcome::Diagnostic { message: e }),
    };
    let (mut stdout, mut stderr) = host.streams();
    let code = emit_turn(turn, tool, &mut stdout, &mut stderr).await;
    host.exit(code);
    Ok(())
}

/// What a Stop or PostCompact reads, starts, writes and ends with.
pub(crate) trait Host {
    /// The host clock now, in unix milliseconds.
    fn now_ms(&self) -> Ms;
    /// The hook input, as the harness wrote it.
    fn input(&mut self) -> Result<String, String>;
    fn spawner(&self) -> &dyn Spawn;
    fn streams(&mut self) -> (&mut dyn std::io::Write, &mut dyn std::io::Write);
    fn exit(&mut self, code: i32);
    /// Whether this process still runs the installed build.
    fn authority(&self) -> crate::upgrade::Authority;
    /// Become the installed build, resuming from the handoff at
    /// `handoff`. Returns only if that failed.
    fn become_installed(&self, handoff: &Path) -> std::io::Error;
}

/// A Stop's input and how long its wait may be held: read from the
/// harness, or handed over by the build this process replaced.
struct Arrived {
    raw: String,
    input: HookInput,
    /// The report the replaced build armed its park with.
    resumed: Option<Report>,
    /// When the hook can no longer hold its wait.
    poll_deadline: std::time::Instant,
    /// When this hook started, by the host clock. `None` for a resumed
    /// one, whose report already carries the time its hook started at.
    hook_started: Option<Ms>,
}

impl Arrived {
    fn fresh(raw: String, started: std::time::Instant, hook_started: Ms) -> Result<Self, String> {
        Ok(Arrived {
            input: parse_input(&raw)?,
            raw,
            resumed: None,
            poll_deadline: poll_deadline(started),
            hook_started: Some(hook_started),
        })
    }

    /// The deadline is the report's own, and never later than a hook
    /// started now would get: whatever the handoff says, the runner's
    /// ceiling is not outstayed.
    fn resumed(handoff: super::hook_handoff::Handoff) -> Result<Self, String> {
        let now = std::time::Instant::now();
        let ceiling = poll_deadline(now);
        let left = std::time::Duration::from_millis(
            handoff
                .report
                .deadline()
                .saturating_sub(super::turn_custody::now_ms()),
        );
        Ok(Arrived {
            input: parse_input(&handoff.input)?,
            raw: handoff.input,
            poll_deadline: now.checked_add(left).map_or(ceiling, |by| by.min(ceiling)),
            resumed: Some(handoff.report),
            hook_started: None,
        })
    }
}

/// What a Stop reports of the moment it arrived.
enum Evidence {
    /// Read from the agent's record by this process, on arrival.
    Read(TurnPlace, Vec<super::question_state::Observation>),
    /// The report the replaced build made of it.
    Carried(Report),
}

fn parse_input(raw: &str) -> Result<HookInput, String> {
    serde_json::from_str(raw).map_err(|e| format!("hook: parsing stdin JSON: {e}"))
}

/// This process: its stdio, `clank wait --turn-end` children, its exit.
pub(crate) struct ProcessHost {
    stdout: std::io::Stdout,
    stderr: std::io::Stderr,
}

impl ProcessHost {
    pub(crate) fn new() -> Self {
        ProcessHost {
            stdout: std::io::stdout(),
            stderr: std::io::stderr(),
        }
    }
}

impl Host for ProcessHost {
    fn now_ms(&self) -> Ms {
        super::turn_custody::now_ms()
    }
    fn input(&mut self) -> Result<String, String> {
        read_stdin()
    }
    fn authority(&self) -> crate::upgrade::Authority {
        crate::upgrade::process().clone()
    }
    fn become_installed(&self, handoff: &Path) -> std::io::Error {
        crate::upgrade::exec_installed(super::hook_handoff::resumed_args(
            std::env::args_os().skip(1),
            handoff,
        ))
    }
    fn spawner(&self) -> &dyn Spawn {
        &Spawner
    }
    fn streams(&mut self) -> (&mut dyn std::io::Write, &mut dyn std::io::Write) {
        (&mut self.stdout, &mut self.stderr)
    }
    fn exit(&mut self, code: i32) {
        crate::cli::source_group::exit(code)
    }
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

/// The Stop, or the park a compaction ended, from routing to the verdict.
async fn stop(
    tool: Tool,
    repo_override: Option<&Path>,
    arrived: Arrived,
    compacted: bool,
    host: &impl Host,
) -> Turn {
    let Arrived {
        raw,
        input,
        resumed,
        poll_deadline,
        hook_started,
    } = arrived;
    // Read now, before any waiting: where in the agent's record this Stop
    // came from, not wherever the record has got to by delivery. A
    // resumed hook is past that moment, and carries what was read then.
    let evidence = match resumed {
        Some(report) => Evidence::Carried(report),
        None => {
            let place = super::wake_repeat::place(
                tool,
                input.transcript_path.as_deref().map(Path::new),
                input.turn_id.as_deref(),
            );
            let questions = question_evidence(tool, &input, &place);
            Evidence::Read(place, questions)
        }
    };
    let repo = match resolve_hook_repo(repo_override, &input) {
        Ok(r) => r,
        Err(message) => return Turn::said(HookOutcome::Diagnostic { message }),
    };
    let label = match resolve_identity_for_hook_in(&repo, tool, &input.session_id) {
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
    let report = match evidence {
        Evidence::Carried(report) => report,
        Evidence::Read(place, questions) => {
            let stop = TurnEndReport {
                tool,
                session: input.session_id.clone(),
                place,
                tasks: TaskSnapshot::of(&input),
                hook_started,
            };
            let deadline = unix_ms(poll_deadline);
            // A compaction ends no turn, so it is no evidence about questions.
            match compacted {
                true => Report::Compacted { stop, deadline },
                false => Report::TurnEnd {
                    stop,
                    deadline,
                    questions,
                },
            }
        }
    };
    let mut life = super::hook_owner::lifetime_for(tool, &super::hook_owner::Native);
    let handoff = super::hook_handoff::Handoff::new(raw, report.clone());
    let mut replacing = crate::upgrade::Replacing::new(host.authority(), |_| {
        if let Err(why) = super::monitor::ensure_runtime_dir() {
            return std::io::Error::other(why);
        }
        let dir = super::monitor::runtime_dir(rustix::process::getuid().as_raw());
        handoff.hand_to(&dir, |path| host.become_installed(path))
    });
    let carried = await_verdict_under_deadline(
        verdict_deadline(poll_deadline),
        tool,
        true,
        &mut life,
        &mut replacing,
        || {
            host.spawner().spawn(
                &binding.repo,
                &binding.label,
                &binding.session,
                tool,
                &report,
            )
        },
    )
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
/// them. Taken from the one reading of the record the Stop was placed by,
/// and only where that reading found the turn's own boundary.
fn question_evidence(
    tool: Tool,
    input: &HookInput,
    place: &TurnPlace,
) -> Vec<super::question_state::Observation> {
    use super::question_state::Observation;
    let (Tool::Claude, Some(turn), Some(_), Some(transcript)) =
        (tool, &place.turn, place.position, &input.transcript_path)
    else {
        return Vec::new();
    };
    let session = input.session_id.as_str().to_string();
    vec![
        Observation::TurnOver {
            session: session.clone(),
            prompt: turn.clone(),
        },
        Observation::Behind {
            session,
            older: super::question_state::prompts_behind(Path::new(transcript), turn),
        },
    ]
}

/// How long a subagent's hook waits to hear its report was recorded. The
/// subagent's stop, or its start, is held for no longer.
const SUBAGENT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// What a subagent's hook reports of `raw`, started at `hook_started`:
/// nothing when the payload names no subagent. A stop carries the
/// session's listing and where the agent's own record stood; a start
/// carries neither. Nothing is read of the subagent's own transcript.
fn subagent_report(
    tool: Tool,
    event: super::SubagentArg,
    raw: &str,
    hook_started: Ms,
) -> Result<Option<(HookInput, ChildReport)>, String> {
    let input = parse_input(raw)?;
    let Some(child) = input.agent_id.clone() else {
        return Ok(None);
    };
    let seen = match event {
        super::SubagentArg::Start => ChildSeen::Started,
        super::SubagentArg::Stop => ChildSeen::Stopped {
            tasks: TaskSnapshot::of(&input),
            agent: super::wake_repeat::place(
                tool,
                input.transcript_path.as_deref().map(Path::new),
                input.turn_id.as_deref(),
            ),
        },
    };
    let report = ChildReport {
        session: input.session_id.clone(),
        child,
        hook_started: Some(hook_started),
        seen,
    };
    Ok(Some((input, report)))
}

/// SubagentStop or SubagentStart: report it, and exit 0 with nothing on
/// stdout. Only a diagnostic is ever written, to stderr.
async fn run_subagent(
    tool: Tool,
    repo_override: Option<&Path>,
    event: super::SubagentArg,
    hook_started: Ms,
    host: &mut impl Host,
) {
    let said = match host.input() {
        Ok(raw) => subagent(tool, repo_override, event, &raw, hook_started, &*host).await,
        Err(e) => Some(e),
    };
    if let Some(message) = said {
        let (_, stderr) = host.streams();
        let _ = writeln!(stderr, "{message}");
    }
    host.exit(HOOK_OK_EXIT);
}

/// Carry a subagent's report under its session's label, and return the
/// diagnostic to print, if any. Whether that session still holds the
/// label is the monitor's to judge.
async fn subagent(
    tool: Tool,
    repo_override: Option<&Path>,
    event: super::SubagentArg,
    raw: &str,
    hook_started: Ms,
    host: &impl Host,
) -> Option<String> {
    let (input, seen) = match subagent_report(tool, event, raw, hook_started) {
        Ok(Some(reported)) => reported,
        Ok(None) => return None,
        Err(e) => return Some(e),
    };
    let repo = match resolve_hook_repo(repo_override, &input) {
        Ok(repo) => repo,
        Err(e) => return Some(e),
    };
    let label = match resolve_identity_for_hook_in(&repo, tool, &input.session_id) {
        Ok(label) => label,
        Err(e) => return Some(format!("{e:#}")),
    };
    let session = input.session_id.as_str();
    let report = Report::Child {
        tool,
        seen,
        deadline: super::turn_custody::now_ms() + SUBAGENT_BUDGET.as_millis() as u64,
    };
    let frame = answer_with(
        host.spawner(),
        &repo,
        &label,
        session,
        report,
        SUBAGENT_BUDGET,
        |_| None,
    )
    .await;
    match frame.verdict {
        WireVerdict::Diagnostic { message } => Some(message),
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

/// Who this session is, and — when it is unbound and clank launched it —
/// the binding that says so.
///
/// A bound session keeps its binding: on `resume`/`compact` that is the
/// same label. An unbound one takes `launched_as`, which the caller passes
/// only when this hook runs in its session's own launch: on `startup` the
/// first bind, on `clear` the rebind that keeps a session tracked across a
/// new id. Any other unbound session waits for `clank as`.
fn session_started(
    repo: &Path,
    tool: Tool,
    input: &SessionStartInput,
    launched_as: Option<&AgentLabel>,
) -> Option<(AgentLabel, Tool)> {
    use crate::agent_env::Start;
    let sid = clank_core::ids::SessionId::parse(&input.session_id).ok()?;
    let label = match crate::agent_env::session_start(repo, tool, &sid, launched_as) {
        Ok(Start::Bound(label)) => {
            if tool == Tool::Claude
                && let Err(e) = crate::agent_store::resume_checked_in(repo, &label, &sid)
            {
                eprintln!("clank: cannot record that this resume checked in: {e:#}");
            }
            return Some((label, tool));
        }
        Ok(Start::Unbound | Start::HeldElsewhere) => return None,
        Ok(Start::Bootstrap(label)) => label,
        Err(e) => {
            eprintln!("clank: cannot decide this session start: {e:#}");
            return None;
        }
    };
    let binding_tool = tool;
    // Fail-open by POLICY: a bind that fails leaves the previous binding,
    // and this session stays unbound; the diagnostic names what did not
    // happen.
    if let Err(e) = crate::agent_store::bind_session_to_agent(repo, &label, binding_tool, &sid) {
        eprintln!(
            "clank: could not bind `{}` to {} session {} ({}): {e:#}",
            label.as_str(),
            binding_tool.as_str(),
            sid.as_str(),
            input.source.as_deref().unwrap_or("start"),
        );
        return None;
    }
    if let Some(path) = input.transcript_path.as_deref() {
        let _ = crate::agent_store::record_transcript(repo, &label, &sid, path);
    }
    Some((label, binding_tool))
}

/// The `CLANK_AGENT` this hook inherited, when its environment is its
/// session's own launch.
fn launch_label(
    tool: Tool,
    env: &dyn Ambient,
    parent: &impl super::hook_owner::Parentage,
) -> Option<AgentLabel> {
    let named = crate::agent_env::explicit_label_in(env).ok().flatten()?;
    super::hook_owner::runs_in_its_launch(tool, parent).then_some(named)
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
    let launched_as = launch_label(tool, &Process, &super::hook_owner::Native);
    let Some((label, binding_tool)) = session_started(&repo, tool, &input, launched_as.as_ref())
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
        &mut crate::upgrade::Replacing::never(),
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

/// Carry `report`, which never parks, to the monitor, and hand `emit` its
/// verdict, or the one its tool takes when none comes within `budget`.
/// `emit` says how writing it out went, or `None` when it wrote nothing; a
/// presentation is answered by that, and is delivered only by a write
/// that succeeded.
pub(super) async fn answer_once(
    repo: &Path,
    label: &AgentLabel,
    session: &str,
    report: Report,
    budget: std::time::Duration,
    emit: impl FnOnce(&Frame) -> Option<std::io::Result<()>>,
) {
    answer_with(&Spawner, repo, label, session, report, budget, emit).await;
}

async fn answer_with(
    spawner: &dyn Spawn,
    repo: &Path,
    label: &AgentLabel,
    session: &str,
    report: Report,
    budget: std::time::Duration,
    emit: impl FnOnce(&Frame) -> Option<std::io::Result<()>>,
) -> Frame {
    let tool = report.tool();
    let carried = await_verdict_under_deadline(
        std::time::Instant::now() + budget,
        tool,
        false,
        &mut super::hook_owner::Lifetime::Unbound,
        &mut crate::upgrade::Replacing::never(),
        || spawner.spawn(repo, label, session, tool, &report),
    )
    .await;
    match carried {
        Ok((frame, wait)) => {
            let emitted = emit(&frame);
            let result = result_of(&frame, emitted, "it was not written out");
            Box::new(wait).answer(result).await;
            frame
        }
        Err(frame) => {
            emit(&frame);
            frame
        }
    }
}

/// The one result for the presentation `frame` hands over, if it hands one
/// over, by how writing it out went; `unwritten` says why when it was not.
fn result_of(
    frame: &Frame,
    emitted: Option<std::io::Result<()>>,
    unwritten: &str,
) -> Option<TurnResult> {
    frame.presentation().map(|id| match emitted {
        Some(Ok(())) => TurnResult::Delivered {
            presentation: id.clone(),
        },
        Some(Err(e)) => TurnResult::EmitFailed {
            presentation: id.clone(),
            why: e.to_string(),
        },
        None => TurnResult::EmitFailed {
            presentation: id.clone(),
            why: unwritten.into(),
        },
    })
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
    let result = result_of(&frame, emitted, "the session no longer holds the label");
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
use crate::proc_run::{Identity, Out, Spec, Stdin};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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

fn wait_command(exe: &Path, repo: &Path, label: &AgentLabel, session: &str, tool: Tool) -> Spec {
    let key = match tool {
        Tool::Claude => "CLAUDE_CODE_SESSION_ID",
        Tool::Codex => "CODEX_THREAD_ID",
        Tool::OpenCode => "OPENCODE_SESSION_ID",
        Tool::Grok => "GROK_AGENT",
    };
    Spec::new(exe)
        .args(wait_argv(repo, label, session))
        // Piped stdin IS the ownership channel, and the report's way in.
        .stdin(Stdin::Piped)
        .stdout(Out::Piped)
        .stderr(Out::Piped)
        .identity(Identity::ScrubInherited)
        .env(key, session)
}

/// What starts the child that carries a report.
pub(crate) trait Spawn {
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
        OwnedWait::spawn(wait_command(&exe, repo, label, session, tool), report)
    }
}

/// A spawned child bundled with the owner handle whose LIFETIME is the
/// ownership contract.
///
/// The handle is a required field, so a child with no owner cannot be
/// constructed, and it is released only by [`Answer::answer`] or
/// [`OwnedWait::shutdown`], after the result it carries is written.
/// Closing it early would read as instant owner death.
pub(crate) struct OwnedWait {
    /// `None` for a carrier running in this process.
    child: Option<crate::proc_run::Managed>,
    owner_end: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    /// The child's verdicts, one per line, read while it stays alive.
    frames: tokio::io::Lines<tokio::io::BufReader<Box<dyn tokio::io::AsyncRead + Unpin + Send>>>,
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
    fn spawn(cmd: Spec, report: &Report) -> Result<Self, String> {
        let mut child =
            crate::proc_run::start(cmd).map_err(|e| format!("hook: spawning wait failed: {e}"))?;
        let owner_end = child.take_stdin_async().ok_or_else(|| {
            "hook: wait spawned without a stdin pipe — the ownership sentinel would be absent, \
             so refusing to run it"
                .to_string()
        })?;
        let stdout = child
            .take_stdout_async()
            .ok_or_else(|| "hook: wait spawned without a stdout pipe".to_string())?;
        let stderr = child.take_stderr_async();
        Self::over(
            Some(child),
            Box::new(owner_end),
            Box::new(stdout),
            stderr.map(|e| Box::new(e) as _),
            report,
        )
    }

    /// The wait whose carrier is reached through these pipes: the owner
    /// pipe it reads the report and result from, and the one it answers on.
    pub(crate) fn over(
        child: Option<crate::proc_run::Managed>,
        owner_end: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        frames: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        mut stderr: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>,
        report: &Report,
    ) -> Result<Self, String> {
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
            frames: tokio::io::BufReader::new(frames).lines(),
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
        if let Some(child) = child.as_mut() {
            let _ = tokio::time::timeout(ABANDON_GRACE, child.wait_async()).await;
        }
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

/// When the hook stops waiting for a verdict at all, given when its
/// report says it can no longer hold the wait.
fn verdict_deadline(poll_deadline: std::time::Instant) -> std::time::Instant {
    poll_deadline + VERDICT_GRACE
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
///
/// While no terminal verdict has been read the hook is parked, holding
/// nothing but its report, and that is where `replacing` may turn it into
/// the installed build. A verdict in hand always goes first: once one is
/// read this process emits it, answers for it and exits by itself.
async fn await_verdict_under_deadline<W: Run>(
    deadline: std::time::Instant,
    tool: Tool,
    parks: bool,
    life: &mut Lifetime,
    replacing: &mut crate::upgrade::Replacing<
        impl FnMut(&crate::build_stamp::Build) -> std::io::Error,
    >,
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
    let parked = async {
        let verdict = wait.verdict_until(life.ended());
        tokio::pin!(verdict);
        tokio::select! {
            biased;
            ran = &mut verdict => ran,
            never = replacing.parked() => match never {},
        }
    };
    let ran = under_deadline(parked, deadline).await;
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

fn read_stdin() -> Result<String, String> {
    let mut buf = String::new();
    match std::io::stdin().read_to_string(&mut buf) {
        Ok(_) => Ok(buf),
        Err(e) => Err(format!("hook: reading stdin: {e}")),
    }
}

pub(crate) fn read_hook_stdin() -> Result<HookInput, String> {
    parse_input(&read_stdin()?)
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
