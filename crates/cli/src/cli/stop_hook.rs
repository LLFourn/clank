//! `clank stop-hook` — the hook adapters `clank setup` installs into
//! `~/.claude/settings.json` and `~/.codex/hooks.json`.
//!
//! TWO adapters live here, for two different hook events with two
//! different contracts. Each is a short algorithm, and the code below
//! is those algorithms in order.
//!
//! # A Stop turn
//!
//! 1. **Identify.** Repo from `--repo`, else the stdin `cwd`; label
//!    from the environment, else the binding. If the binding no longer
//!    names this session and this tool, be Silent — someone else is
//!    this label now.
//! 2. **Gather.** Reads with no effects, all of them taken before
//!    anything decides: auto mode (absent means "unset, inherit the
//!    `~/.clank` default", never "off"), role, the attendance marker,
//!    and whether that marker's recorded process is provably still the
//!    process it recorded.
//! 3. **Decide** — [`decide`], one pure function of that evidence. The
//!    ORDER is the content: an unreadable config outranks auto-off,
//!    auto-off outranks an unresolvable role, and only an agent that is
//!    driving ever hears about its roster.
//! 4. **Leave** — [`leave_projection`] applies the plan's
//!    [`Projection`] to the marker and the decision record, once,
//!    before any arm returns, so no exit can skip it.
//! 5. **Run** the one effect the decision names: the hook-owned wait,
//!    the check-in timer, or nothing.
//! 6. **Re-check the binding** before emitting. A park can outlive the
//!    incarnation that armed it.
//! 7. **Render** — [`emit_and_exit`] writes the tool's wire shape.
//!
//! Its whole state is three files under `.clank/agents/<label>/`, and
//! there is no fourth: `attending` (the agent's intent for ONE yield,
//! written by `clank run` / `clank attending`, consumed here),
//! `attended` (the projection `clank status` and the TUI read), and
//! `attending.holder` (the identity of an armed check-in timer).
//!
//! # A SessionStart (`--session-start`)
//!
//! A different event with a different contract, so it does not share
//! the steps above: read stdin; bind the session clank launched to its
//! label, or identify one it did not launch by its id; stop if auto is
//! off; peek for pending work; and if there is any, and the binding
//! still holds, surface it on `additionalContext` so a resumed session
//! catches up instead of waiting for its first turn-end.
//!
//! **Fail-open at every step, by policy:** a session start must never
//! be broken by clank state, so every error path exits 0 and the
//! session starts regardless. Most return QUIETLY and are meant to.
//! The only paths that speak are the two bind-side failures in
//! [`session_started`] — a roster tool that will not resolve, and a
//! bind that did not land — where the diagnostic names what did not
//! happen and the previous binding survives.
//!
//! # The exit contracts
//!
//! They are two, and they share only "never fail the agent". Reading
//! one as the other is what merging the adapters into a single
//! paragraph invites.
//!
//! **A Stop turn.** Exit 2 is Claude's continuation channel and
//! NOTHING else: a `Continue` for Claude exits
//! [`CLAUDE_CONTINUATION_EXIT`], which is how work reaches the
//! session. Every other outcome — Silent, Diagnostic, Codex's block
//! JSON, OpenCode's renew wire — exits [`HOOK_OK_EXIT`]. The
//! invariant is about FAILURE, not about zero: **no Stop error path
//! exits non-zero**, because every one of them becomes a
//! [`HookOutcome::Diagnostic`] that exits 0 with a human-readable
//! stderr line. A non-zero exit from any Stop path but Claude's
//! continuation is a clank bug.
//!
//! **A SessionStart.** It produces no [`HookOutcome`] at all, so it
//! has no Diagnostic and no continuation: every path exits 0. Its
//! errors return QUIETLY, and the two bind-side failures that speak
//! print to stderr and still exit 0.
//!
//! # The poll
//!
//! It has THREE terminators: a wake, its own deadline, or the hook's
//! OWN death. The deadline is derived from the ceiling clank writes
//! into the hook config and expires cleanly BELOW it, because being
//! killed at the ceiling surfaces to the user as a failed hook
//! (codex-idle-is-not-a-hook-failure). Death it learns by holding the
//! write end of the wait's stdin pipe (remove-wait-timeout): if this
//! process is killed — SIGKILL included, where no destructor runs —
//! the pipe closes and the wait reaps itself.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::{StopHookArgs, resolve_repo};
use crate::agent_env::{Ambient, Process, resolve_identity_for_hook_in};
use crate::agent_store::load_agent_config;
use crate::lifecycle::AgentLabel;
use clank_core::{
    AutoMode, CLAUDE_CONTINUATION_EXIT, CodexBlockDecision, HOOK_OK_EXIT, HookInput, HookOutcome,
    Role, SilentReason, Tool,
};

pub async fn run(args: StopHookArgs) -> anyhow::Result<()> {
    if args.session_start {
        return run_session_start(args).await;
    }
    // The runner's ceiling clock starts HERE, at process spawn — so
    // the poll's own deadline must be measured from here too, not from
    // where the waiting begins.
    let started = std::time::Instant::now();
    let tool: Tool = args.tool.into();
    let outcome = match read_hook_stdin() {
        Ok(input) => stop_input(&args, input, started, &Process).await,
        Err(e) => HookOutcome::Diagnostic { message: e },
    };
    emit_and_exit(outcome, tool);
}

/// The decoded Stop turn, shared by the installed hook and tests.
async fn stop_input(
    args: &StopHookArgs,
    input: HookInput,
    started: std::time::Instant,
    env: &dyn Ambient,
) -> HookOutcome {
    let tool: Tool = args.tool.into();
    let async_loop =
        tool == Tool::Claude && args.loop_mode == Some(crate::cli::LoopModeArg::Asyncrewake);
    compute_outcome_with(tool, args.repo.as_deref(), input, async_loop, started, env).await
}

/// Test-facing wrapper: production routes through
/// [`compute_outcome_with`] carrying the argv-selected loop mode.
#[cfg(test)]
async fn compute_outcome(
    tool: Tool,
    repo_override: Option<&Path>,
    input: HookInput,
) -> HookOutcome {
    compute_outcome_in(
        tool,
        repo_override,
        input,
        &crate::agent_env::Stated::default(),
    )
    .await
}

/// The same, for a test that wants to STATE an environment — an
/// exported `CLANK_AGENT`, say — rather than the empty one.
#[cfg(test)]
async fn compute_outcome_in(
    tool: Tool,
    repo_override: Option<&Path>,
    input: HookInput,
    env: &dyn Ambient,
) -> HookOutcome {
    compute_outcome_with(
        tool,
        repo_override,
        input,
        false,
        std::time::Instant::now(),
        env,
    )
    .await
}

async fn compute_outcome_with(
    tool: Tool,
    repo_override: Option<&Path>,
    input: HookInput,
    async_loop: bool,
    started: std::time::Instant,
    env: &dyn Ambient,
) -> HookOutcome {
    let live_ids = input.live_task_ids();

    // Resolve repo: explicit --repo (testing) > hook stdin cwd.
    let repo = match resolve_hook_repo(repo_override, &input) {
        Ok(r) => r,
        Err(e) => return outcome_of(&TurnDecision::Failed(e)),
    };

    let label = match resolve_identity_for_hook_in(&repo, tool, &input.session_id, env) {
        Ok(l) => l,
        Err(e) => return outcome_of(&TurnDecision::Failed(format!("{e:#}"))),
    };

    if !crate::agent_store::binding_matches(&repo, &label, tool, input.session_id.as_str()) {
        return HookOutcome::Silent {
            why: SilentReason::StaleBinding,
        };
    }
    let agent_dir = crate::agent_store::agents_root(&repo).join(label.as_str());

    // ONE EXIT, so what is reported is what the turn actually did.
    // The turn used to record its INTENT before parking, and nothing
    // updated that record once the wait returned — so a hook that
    // ended by handing an agent work still described itself as
    // waiting, and that agent was reported unreachable (codex
    // 01cd4b3f1a39).
    let outcome = async {
        // Evidence, not effects: each of these is a read that says
        // nothing about who owns what, so they can all be taken before
        // anything decides. The errors travel IN the results and are
        // inspected in the order `decide` states, so a higher-priority
        // branch still wins (codex 31b4b49f).
        //
        // Config may be ABSENT (bound via `clank as` but auto never
        // touched) — that is not "off", it is "unset, inherit the
        // ~/.clank default" (auto-mode-default-on), which the shared
        // resolver answers (codex b29d8d0).
        let attending = read_attending(&agent_dir);
        let auto = load_agent_config(&repo, &label)
            .map(|cfg| {
                let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref())
            })
            .map_err(|e| format!("{e:#}"));
        let role = crate::agent_store::resolve_role(&repo, &label).map_err(|e| {
            format!(
                "hook: cannot resolve `{}`'s role ({e:#}); arming no wait. \
                 Fix the roster and the next turn-end will arm one.",
                label.as_str()
            )
        });
        // Attendance is proven HERE, before any park, because this is
        // where `live_ids` is FRESH. Proving it at wait-return meant
        // judging a snapshot taken at hook entry and carried through an
        // unbounded park — by then the attended task may have finished
        // hours ago and spent its wake, and suppressing on that is the
        // permanent sleep (codex on 81981f2).
        let proven = prove_attendance(attending.as_ref(), &input);

        let plan = decide(TurnEvidence {
            tool,
            async_loop,
            auto,
            role,
            proven,
        });

        // Both files, once, from the plan — before any arm returns, so
        // no exit can skip it. `AutoMode::Off` and the empty-items path
        // both used to exit above the old reap, which is how markers
        // outlived their turn.
        leave_projection(&agent_dir, plan.projection());

        let (role, route) = match plan {
            TurnPlan::Decided(decision) => {
                return outcome_of(&decision);
            }
            TurnPlan::AttendedYield(proven) => {
                // The proof is in hand HERE — a pid the stored token
                // still holds — so the decision is built here and the
                // label falls out of it.
                return outcome_of(&attended_yield(&proven));
            }
            TurnPlan::Attend(proven) => {
                return outcome_of(
                    &attend(
                        &proven,
                        &agent_dir,
                        poll_deadline(started),
                        || proven.rec.work_alive(),
                        || {
                            crate::agent_store::binding_matches(
                                &repo,
                                &label,
                                tool,
                                input.session_id.as_str(),
                            )
                        },
                    )
                    .await,
                );
            }
            TurnPlan::Drive(role, route, _) => (role, route),
        };

        match route {
            TurnRoute::Wait => {
                compute_wait_outcome(
                    &repo,
                    &label,
                    role,
                    started,
                    &live_ids,
                    deadline_action(tool),
                    input.session_id.as_str(),
                    tool,
                )
                .await
            }
            TurnRoute::Undrivable => outcome_of(&TurnDecision::Failed(
                "hook: grok has no stop-hook adapter (grok hooks are passive)".into(),
            )),
        }
    }
    .await;

    if !crate::agent_store::binding_matches(&repo, &label, tool, input.session_id.as_str()) {
        return HookOutcome::Silent {
            why: SilentReason::StaleBinding,
        };
    }
    outcome
}

/// Everything the turn-end decision reads, gathered before any of it
/// is consulted.
///
/// Gathering could not happen up front while the reads were welded to
/// effects — one call read AND deleted the attendance marker, another
/// proved AND recorded it, so each had to sit at an exact point in
/// the control flow. With those split the evidence is just values,
/// and `decide` is a function of them.
struct TurnEvidence<'a> {
    tool: Tool,
    async_loop: bool,
    /// The config error travels HERE rather than returning at the
    /// read, so a higher-priority branch still wins (codex 31b4b49f).
    auto: Result<AutoMode, String>,
    role: Result<Role, String>,
    proven: Option<Proven<'a>>,
}

/// What the turn does, once everything it reads has been read.
enum TurnPlan<'a> {
    /// Decided outright; nothing left to run.
    Decided(TurnDecision),
    /// Hold the check-in timer for this proven attendance.
    Attend(Proven<'a>),
    /// Yield to a proven attendance without holding a timer: the
    /// legacy synchronous hook cannot outlive the turn. Carries the
    /// proof because the projection is recorded either way — the
    /// timer is what differs, not the attendance.
    AttendedYield(Proven<'a>),
    /// Drive as this role, by this route — and say what became of an
    /// attendance this turn did not act on.
    Drive(Role, TurnRoute, Attendance),
}

/// What a driving turn did about an attendance it did not act on.
///
/// Its own answer, because "did this turn consume the attendance" is
/// not "was this turn a yield" — and reading one off the other is what
/// deleted a marker for a turn that had declined it (grok
/// 36518be9bfd9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attendance {
    /// There was none to act on.
    None,
    /// There was, and this tool cannot deliver its completion wake.
    /// DECLINED, not consumed: the work is still running, and the
    /// record still describes it.
    Declined,
}

/// What the turn leaves in the agent's two attendance files.
///
/// The marker and the projection move TOGETHER — a turn that takes
/// the marker also replaces what the projection says about it — so
/// one value decides both and one function applies it. It used to be
/// a `takes_marker()` bool that deleted both files, plus two
/// `record_attended` calls putting one of them back, so `attended`
/// briefly said the wrong thing and two call sites had to agree about
/// when.
enum Projection<'a> {
    /// Yielded to this attendance: take the marker and record the
    /// decision. The record is all a human ever sees of it — the
    /// marker is gone within the turn, which is too short to observe.
    Attended(&'a Attending),
    /// Took the marker with nothing to show for it. What remains must
    /// describe THIS turn-end and no earlier one.
    Empty,
    /// DECLINED: this tool cannot deliver the completion wake, which
    /// says nothing about the work. It is still running and the record
    /// still describes it, so neither file moves.
    Untouched,
}

impl<'a> TurnPlan<'a> {
    /// What this turn leaves behind.
    ///
    /// Was the ORDER of two statements — a yield returned above the
    /// consume and left it, everything below took it — which is not
    /// something a reader can check or a test can ask.
    fn projection(&self) -> Projection<'a> {
        match self {
            TurnPlan::Attend(proven) | TurnPlan::AttendedYield(proven) => {
                Projection::Attended(proven.rec)
            }
            TurnPlan::Drive(_, _, Attendance::Declined) => Projection::Untouched,
            TurnPlan::Decided(_) | TurnPlan::Drive(_, _, Attendance::None) => Projection::Empty,
        }
    }
}

/// The whole turn-end decision, as one function of the evidence.
///
/// Every branch here was a `return` at a different depth of
/// `compute_outcome_with`, so the only way to ask what a turn decides
/// was to build a repo that reached that depth.
fn decide(ev: TurnEvidence<'_>) -> TurnPlan<'_> {
    let role = match drive_gate(&ev.auto, &ev.role) {
        Err(decision) => return TurnPlan::Decided(decision),
        Ok(role) => role,
    };
    // An attendance is a waker only where a completion wake exists.
    // For an in-hook adapter nothing delivers one, so yielding to the
    // task would be the same silence under a better name — it routes
    // to the hook's wait instead.
    let declined = if ev.proven.is_some() && !wakes_on_background_completion(ev.tool) {
        Attendance::Declined
    } else {
        Attendance::None
    };
    if let Some(proven) = ev
        .proven
        .filter(|_| wakes_on_background_completion(ev.tool))
    {
        // Only a hook that outlives the turn can hold the check-in
        // timer. The legacy claude hook is synchronous — five minutes
        // inside it would freeze the session — so it yields at once.
        return if ev.async_loop {
            TurnPlan::Attend(proven)
        } else {
            TurnPlan::AttendedYield(proven)
        };
    }
    TurnPlan::Drive(role, route(ev.tool), declined)
}

/// What a driving turn does next.
///
/// Effects, named — so choosing between them is a value the tests can
/// read, and the effect itself is the caller's to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnRoute {
    /// Run the hook-owned long poll. Every adapter that can be driven
    /// takes this one path, in either loop mode — the asyncrewake
    /// "park" was a second name for it, with the same call behind it.
    Wait,
    /// Nothing can drive this tool.
    Undrivable,
}

/// Claude reports completion of an attended task to its session.
/// The other adapters rely on the hook-owned wait for a continuation.
fn wakes_on_background_completion(tool: Tool) -> bool {
    match loop_policy(tool) {
        LoopPolicy::TaskCompletionWake => true,
        LoopPolicy::InHookWait | LoopPolicy::Passive => false,
    }
}

/// Route a driving turn to its hook-owned wait.
///
/// The loop mode is NOT an input. `--loop asyncrewake` says the hook
/// may outlive the turn, and that changes exactly one thing: whether
/// a proven attendance holds the check-in timer. It never changed
/// where a driving turn goes, though it used to select an arm that
/// spelled the same call differently.
fn route(tool: Tool) -> TurnRoute {
    match loop_policy(tool) {
        LoopPolicy::TaskCompletionWake => TurnRoute::Wait,
        // The in-hook long-poll + emit-with-items model: codex blocks
        // with the items; opencode's plugin injects work on
        // session.idle and renews an expired wait without a model
        // turn. A prompt on expiry would loop on that next idle.
        LoopPolicy::InHookWait => TurnRoute::Wait,
        // Grok's hooks are PASSIVE (grok-first-class): clank installs
        // no grok adapter and no continuation could drive it. If
        // something wires this up anyway, say so.
        LoopPolicy::Passive => TurnRoute::Undrivable,
    }
}

/// Whether clank drives this turn, and as what.
///
/// PURE, and the order is the whole content: a config that will not
/// parse outranks auto-off, auto-off outranks an unresolvable role,
/// and only an agent that is driving ever hears about its roster.
/// Each of those was a position in a run of returns, which is a thing
/// you can only test by building a repo that reaches it.
///
/// `Err` is the decision the turn ends with; `Ok` is the role it
/// drives as.
fn drive_gate(
    auto: &Result<AutoMode, String>,
    role: &Result<Role, String>,
) -> Result<Role, TurnDecision> {
    match auto {
        Err(e) => return Err(TurnDecision::Failed(e.clone())),
        // The agent asked not to be driven. Diagnosing its roster
        // here would be noise about work it is not going to do, which
        // is why the role error below is unreachable from this arm.
        Ok(AutoMode::Off) => return Err(TurnDecision::NotOurs(NotOurs::AutoOff)),
        Ok(AutoMode::On) => {}
    }
    // A FAILURE is not laundered into an answer. Defaulting to
    // Reviewer was the opposite of the conservative choice it claimed
    // to be: for an agent that is master it arms a poll that can
    // never return, so the session may not hear about its work.
    //
    // So arm nothing, and SAY so. That failure cost hours of dead
    // session undiagnosable from outside; a quiet exit would rebuild
    // exactly that hole.
    role.clone().map_err(TurnDecision::Failed)
}

/// Which continuation model a tool's stop-hook uses — the PURE
/// tool→loop-policy decision (codex fbc5da1), so routing is testable
/// without the spawning wait path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopPolicy {
    /// Claude can wake when an attended background task completes.
    TaskCompletionWake,
    /// In-hook long-poll; non-empty output is the continuation
    /// (codex blocks with it, opencode's plugin injects it).
    InHookWait,
    /// Passive hooks; nothing can drive a continuation (grok).
    Passive,
}

/// What an adapter does when the long-poll reaches its deadline.
///
/// The runner's ceiling cannot be escaped — `asyncRewake` is the only
/// mode that wakes on exit 2 and its timeout is always enforced — so
/// the deadline WILL arrive on a quiet repo. The only choice is what
/// it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlineAction {
    /// Wake with a heartbeat, so the turn-end that follows parks a
    /// fresh hook under a new ceiling. Costs one turn per ceiling
    /// period; the alternative is an agent that never runs again.
    ReArm,
    /// Tell the OpenCode plugin to renew the long-poll without a
    /// model turn. Its idle loop cannot consume a heartbeat prompt.
    RenewWait,
    /// An adapter with no continuation must report the impossible
    /// wait route rather than ending silently.
    Fail,
}

/// EXHAUSTIVE by design: a new adapter must answer this question
/// rather than inherit whichever arm happened to be the default.
fn deadline_action(tool: Tool) -> DeadlineAction {
    match tool {
        Tool::Claude | Tool::Codex => DeadlineAction::ReArm,
        // opencode's plugin runs its work loop on `session.idle`, so a
        // continuation re-idles immediately and spins forever
        // (codex 8000d6e). It needs a different answer, not this one.
        Tool::OpenCode => DeadlineAction::RenewWait,
        // Passive hooks reach no deadline in normal routing.
        Tool::Grok => DeadlineAction::Fail,
    }
}

/// The heartbeat wake.
///
/// It must not read as work. An agent woken with no items and no
/// explanation will look for a reason and invent one, so this says
/// what happened and what the correct response is.
const REARM_REASON: &str = "clank: no work pending — the wait reached its ceiling and is re-arming. \
This is a heartbeat, not a task. End your turn; the next hook parks a fresh wait.";

fn deadline_outcome(action: DeadlineAction) -> HookOutcome {
    match action {
        DeadlineAction::ReArm => HookOutcome::Continue {
            reason: REARM_REASON.to_string(),
        },
        DeadlineAction::RenewWait => HookOutcome::RenewWait,
        DeadlineAction::Fail => HookOutcome::Diagnostic {
            message: "hook: passive adapter reached an in-hook wait deadline".into(),
        },
    }
}

fn loop_policy(tool: Tool) -> LoopPolicy {
    match tool {
        Tool::Claude => LoopPolicy::TaskCompletionWake,
        Tool::Codex | Tool::OpenCode => LoopPolicy::InHookWait,
        Tool::Grok => LoopPolicy::Passive,
    }
}

/// The SessionStart payload both tools send: claude documents it,
/// codex 0.153 carries the same fields (read from the schema in its
/// binary and confirmed with a hook that echoed its stdin).
// ── the SessionStart adapter (a-session-binds-itself-at-start) ──
// A DIFFERENT hook event, sharing this file and nothing else: it does
// not take a Stop turn, touches none of the Stop turn's three files,
// and fails open where the Stop path diagnoses. It runs from here
// through [`peek_items`], which no Stop path calls.
// Kept here rather than extracted: the split that would pay has to
// move ~3000 lines of tests with it, and a test lost in that move is
// invisible, which is a worse failure than a long file.

#[derive(serde::Deserialize)]
struct SessionStartInput {
    session_id: String,
    cwd: String,
    /// `startup` / `resume` / `clear` / `compact`. Absent on a payload
    /// that predates it; nothing here branches on it, it is recorded
    /// for the diagnostic line.
    #[serde(default)]
    source: Option<String>,
    /// Claude sends the session's own transcript file; codex does not.
    /// Recorded on the binding so the remote can read what the agent
    /// said without guessing the path.
    #[serde(default)]
    transcript_path: Option<String>,
}

/// Who this session is, and — when clank launched it — the binding
/// that says so.
///
/// A session clank launched carries `CLANK_AGENT` (set by `clank
/// agent start` after scrubbing any inherited identity), and that
/// label is bound to the id on stdin: on `startup` the first bind,
/// on `clear` the rebind that keeps a session tracked across a new
/// id, on `resume`/`compact` an idempotent re-assertion. The bind
/// records the current session binding. A session clank did not launch has no label in its environment and
/// is identified by its id, as `clank as` bound it
/// (a-session-binds-itself-at-start).
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
            // Fail-open by POLICY: a session start is never broken by
            // clank state. A bind that fails leaves the previous
            // binding, and the Stop hook still resolves this session
            // by its label; the diagnostic names what did not happen.
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
                // Same fail-open policy: a transcript clank cannot
                // record is a page without one, not a session that
                // cannot start.
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

/// SessionStart (claude-asyncrewake-work-loop): every incarnation
/// binds or MINTS — see [`session_started`] — then surfaces pending
/// work via `additionalContext` so a resumed session catches up
/// immediately instead of waiting for its first turn-end park.
/// Fail-open EVERYWHERE: a session start must never be broken by
/// clank state, so every error path exits 0 quietly.
async fn run_session_start(args: StopHookArgs) -> anyhow::Result<()> {
    let mut raw = String::new();
    use std::io::Read as _;
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return Ok(());
    }
    let Ok(input) = serde_json::from_str::<SessionStartInput>(&raw) else {
        return Ok(());
    };
    let Ok(repo) = crate::cli::resolve_repo(Some(Path::new(&input.cwd))) else {
        return Ok(());
    };
    let tool: Tool = args.tool.into();
    let launched_as = crate::agent_env::explicit_label_in(&Process).ok().flatten();
    let Some((label, binding_tool)) =
        session_started(&repo, tool, &input, launched_as.as_ref(), &Process)
    else {
        return Ok(());
    };

    let cfg = load_agent_config(&repo, &label).ok().flatten();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref()) == AutoMode::Off
    {
        return Ok(());
    }
    // No role to get wrong here: the peek resolves it from identity
    // in the subprocess, so this cannot greet a session with another
    // agent's work.
    let Ok(items) = peek_items(&repo, &label).await else {
        return Ok(());
    };
    if items.is_empty() {
        return Ok(());
    }
    // Needed only to PHRASE the items, and only now that there are
    // some. Fail-open like every other path here: a session start is
    // never broken by clank state, so an unresolvable role surfaces
    // nothing rather than guessing whose work this is.
    let Ok(role) = crate::agent_store::resolve_role(&repo, &label) else {
        return Ok(());
    };
    if !crate::agent_store::binding_matches(&repo, &label, binding_tool, &input.session_id) {
        return Ok(());
    }
    println!(
        "{}",
        session_start_payload(&render_wait_items(&items, &label, role))
    );
    Ok(())
}

/// The SessionStart hook-output wire (claude-asyncrewake-work-loop):
/// pending work rides `additionalContext`. Pure — the catch-up seam
/// the acceptance pins.
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

/// The peek's items for SessionStart catch-up context.
/// Self-spawns `clank wait --peek --json` (side-effect-free: fires no
/// hooks) and captures its stdout, never inheriting it into the hook's
/// own protocol output.
async fn peek_items(repo: &Path, label: &AgentLabel) -> Result<Vec<WaitItem>, String> {
    use tokio::process::Command;

    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let output = Command::new(&exe)
        .arg("wait")
        .arg("--peek")
        .arg("--repo")
        .arg(repo)
        .arg("--author")
        .arg(label.as_str())
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("spawning peek failed: {e}"))?;

    match output.status.code() {
        Some(0) => {
            parse_wait_json(&output.stdout).map_err(|e| format!("peek stdout malformed: {e}"))
        }
        Some(3) if output.stdout.is_empty() => Ok(Vec::new()),
        other => Err(format!(
            "peek exited {} stderr={}",
            other
                .map(|c| c.to_string())
                .unwrap_or_else(|| "(signaled)".into()),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

use tokio::process::Command;

/// Argv for the in-hook wait. Separate from the spawn so the shape
/// can be asserted without launching anything.
///
/// Carries no role: `clank wait` derives it from `--author`'s roster
/// entry, per refold, so a roster that changes under a parked wait
/// corrects it on the next wake. The hook could not usefully supply
/// one anyway — a role it resolved once would be exactly the fixed
/// input that left a master's wait blocked forever on reviewer work.
fn wait_argv(repo: &Path, label: &AgentLabel, session: &str) -> Vec<String> {
    vec![
        "wait".into(),
        "--repo".into(),
        repo.display().to_string(),
        "--author".into(),
        label.as_str().to_string(),
        // Ownership, not decoration: without this the wait has no
        // bound at all and outlives a killed hook forever.
        "--die-with-owner".into(),
        "--deliver-to-session".into(),
        session.into(),
        "--json".into(),
    ]
}

fn wait_command(exe: &Path, repo: &Path, label: &AgentLabel, session: &str, tool: Tool) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(wait_argv(repo, label, session))
        // Piped stdin IS the ownership channel. `OwnedWait::spawn`
        // refuses to proceed without it.
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

/// A spawned wait bundled with the owner handle whose LIFETIME is the
/// ownership contract.
///
/// The handle is not an incidental local that must be remembered: it
/// is a required field, so a wait with no owner cannot be
/// constructed, and it is released only inside [`run`](Self::run),
/// after the child is collected. Closing it early would give every
/// in-hook wait instant EOF and exit it on the spot, so the drop
/// point is not something to leave to statement order.
struct OwnedWait {
    child: tokio::process::Child,
    owner_end: tokio::process::ChildStdin,
}

impl OwnedWait {
    /// Spawn `cmd`, requiring the ownership pipe. A missing
    /// `ChildStdin` is a hard error rather than a silently unowned
    /// wait — that degradation is invisible until a hook is killed
    /// and its wait is still running days later.
    fn spawn(cmd: &mut Command) -> Result<Self, String> {
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("hook: spawning wait failed: {e}"))?;
        let owner_end = child.stdin.take().ok_or_else(|| {
            "hook: wait spawned without a stdin pipe — the ownership \
             sentinel would be absent, so refusing to run it"
                .to_string()
        })?;
        Ok(Self { child, owner_end })
    }

    /// Collect the wait, holding the owner handle across the whole
    /// await and dropping it only afterwards.
    async fn run(self) -> std::io::Result<std::process::Output> {
        let Self { child, owner_end } = self;
        let output = child.wait_with_output().await;
        drop(owner_end);
        output
    }
}

/// Margin between the in-hook poll's own deadline and the hook
/// runner's ceiling.
///
/// The runner's clock starts at PROCESS SPAWN, not at the poll, so the
/// margin has to cover everything else this process does: reading
/// stdin, resolving identity, loading config, acquiring the park
/// lease, spawning the child, and emitting. Generous on purpose — the
/// cost of expiring early is one silent exit and a re-park on the next
/// turn end; the cost of expiring late is the runner killing us, which
/// is what the human sees as `hook timed out after 86400s`.
const POLL_MARGIN_SECS: u64 = 300;

/// The ABSOLUTE instant past which this hook must not still be
/// waiting, derived from the ceiling clank itself writes into the
/// tool's hook config so the two cannot drift apart.
///
/// Absolute, not a duration budget: a budget computed before the child
/// spawn and armed after it silently excludes the spawn, which is the
/// same "armed after unbounded setup" dishonesty that got `--timeout`
/// deleted (a 2s timeout measured at 9.5s). An instant fixed at hook
/// entry charges everything that happens afterwards, whenever the
/// timeout is finally armed.
///
/// LIMIT, stated rather than papered over: this bounds the WAIT, not
/// the setup. Nothing here can preempt a hang in identity resolution
/// or lease acquisition — those are not cancellable at this boundary.
/// The deadline is re-checked immediately before arming, so a slow
/// setup shortens or cancels the wait, but a setup that never returns
/// still reaches the runner's ceiling. Making setup itself cancellable
/// is a larger change than this plan.
fn poll_deadline(started: std::time::Instant) -> std::time::Instant {
    let ceiling = std::time::Duration::from_secs(crate::cli::setup::HOOK_TIMEOUT_SECS);
    let margin = std::time::Duration::from_secs(POLL_MARGIN_SECS);
    started + ceiling.saturating_sub(margin)
}

/// Await `fut` until `deadline`. `None` = the deadline won.
///
/// The seam that makes expiry testable without spawning anything: the
/// production path passes the real `OwnedWait::run()`, tests pass a
/// future they control. Dropping `fut` on expiry is load-bearing —
/// that is what closes the owner pipe so the child reaps itself.
async fn under_deadline<F: std::future::Future>(
    fut: F,
    deadline: std::time::Instant,
) -> Option<F::Output> {
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), fut)
        .await
        .ok()
}

/// Long-poll via a self-spawned `clank wait --json`. Used by BOTH
/// loops: codex's blocking hook and claude's asyncrewake park.
/// Reuses the watcher loop without refactoring it. On items →
/// Continue. Anything non-zero → Diagnostic. The poll parks until a
/// wake, its own deadline (which expires BELOW the hook-runner
/// ceiling), or OUR death (remove-wait-timeout).
///
/// The child's stdin is the ownership sentinel, not a spare fd. We
/// hold the write end for the whole wait, so if this hook is killed
/// — SIGKILL included, where no destructor runs and `kill_on_drop`
/// cannot fire — the pipe closes and the wait reaps itself. That is
/// why this spawns and holds the handle instead of calling
/// `Command::output()`, which closes the child's stdin immediately
/// and would read as instant owner death.
async fn compute_wait_outcome(
    repo: &Path,
    label: &AgentLabel,
    role: Role,
    started: std::time::Instant,
    live_ids: &[String],
    on_deadline: DeadlineAction,
    session: &str,
    tool: Tool,
) -> HookOutcome {
    // Fixed at hook entry, so everything below — current_exe, command
    // construction, the child spawn — is charged against it.
    let deadline = poll_deadline(started);
    let arm = || {
        let exe = std::env::current_exe().map_err(|e| format!("hook: current_exe failed: {e}"))?;
        let mut cmd = wait_command(&exe, repo, label, session, tool);
        Ok(OwnedWait::spawn(&mut cmd)?.run())
    };
    let result = await_wait_under_deadline(deadline, on_deadline, arm).await;
    if !crate::agent_store::binding_matches(repo, label, tool, session) {
        return HookOutcome::Silent {
            why: SilentReason::StaleBinding,
        };
    }
    match result {
        Ok(output) => outcome_from_wait_output(&output, label, role, live_ids),
        Err(outcome) => outcome,
    }
}

/// Everything the deadline governs: the pre-check, the arming, and the
/// expiry.
///
/// The wait is INJECTED because this composition is the thing worth
/// testing and the thing that cannot be spawned in a test. Reassembling
/// `under_deadline` and `deadline_outcome` in a test body instead would
/// leave BOTH production exits free to regress to silence while the
/// test stayed green — which is exactly what an earlier version of
/// these tests did.
async fn await_wait_under_deadline<F>(
    deadline: std::time::Instant,
    on_deadline: DeadlineAction,
    arm: impl FnOnce() -> Result<F, String>,
) -> Result<std::process::Output, HookOutcome>
where
    F: std::future::Future<Output = std::io::Result<std::process::Output>>,
{
    // The RARER exit and the strictly worse one: it dies having done no
    // waiting at all, so a setup slow near the boundary must not be
    // what puts the agent to sleep. Short-circuits BEFORE arming.
    if std::time::Instant::now() >= deadline {
        return Err(deadline_outcome(on_deadline));
    }

    let waiting = match arm() {
        Ok(f) => f,
        Err(message) => return Err(HookOutcome::Diagnostic { message }),
    };

    // Expire BELOW the runner's ceiling, cleanly. Dropping the future
    // closes the stdin pipe the wait treats as its owner sentinel, so
    // the child reaps itself — the deadline does not leak a wait, and
    // it does not mask owner death: whichever fires first ends the
    // same way.
    // Re-checked here by construction: the deadline is absolute, so
    // the arming above has already eaten into it.
    match under_deadline(waiting, deadline).await {
        None => Err(deadline_outcome(on_deadline)),
        Some(Ok(o)) => Ok(o),
        Some(Err(e)) => Err(HookOutcome::Diagnostic {
            message: format!("hook: waiting on wait failed: {e}"),
        }),
    }
}

// ── attending a background task (attending-suppresses-standing-wakes) ──

/// Where the agent records the background task it is waiting on.
fn attending_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("attending")
}

/// Read an agent's attendance record STRAIGHT FROM DISK.
///
/// For decisions that must not run against cached state. The TUI's
/// snapshot is refreshed by an asynchronous filesystem wake, so it can
/// lag the file by however long a confirm sits on screen — and a
/// signal decided from a lagging snapshot is a signal decided from a
/// record that may already have been replaced (codex on f2793af).
pub(crate) fn read_attended_now(repo: &Path, label: &str) -> Option<Attended> {
    let path = attended_path(&crate::agent_store::agents_root(repo).join(label));
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Where the hook records the attendance decision it last made.
fn attended_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("attended")
}

/// WHICH live-work signal may satisfy a marker.
///
/// Without this the two correlation paths leak into each other: a
/// hand-written task-id marker could be satisfied by an unrelated
/// `clank run` that happened to share its text, and a run marker by a
/// coincidentally equal harness task id (codex on c566eea). A marker
/// is evidence about one specific thing, so it accepts one specific
/// kind of proof.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Correlation {
    /// `task` is a harness task id. The default, so every record
    /// written before this field existed keeps its old meaning.
    #[default]
    TaskId,
    /// `task` is the `--desc` of a `clank run` clank owns.
    RunDesc,
}

/// How long the attended work was expected to take, and from when.
///
/// The clock starts at the MARKER, not at the turn-end that consumes
/// it: the expectation is about the work, which `clank run` starts
/// the instant after writing the marker. `since` is also the
/// attendance's identity — nanosecond RFC3339, written once, consumed
/// at most once — which is what the check-in holder file records.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expectation {
    pub(crate) since: String,
    pub(crate) secs: u64,
}

/// What a marker carries when the caller said nothing. Long enough
/// for a build, short enough that a hang costs minutes, not an
/// afternoon.
pub(crate) const DEFAULT_EXPECT_SECS: u64 = 300;

impl Expectation {
    pub(crate) fn starting_now(secs: u64) -> Self {
        Self {
            since: now_rfc3339(),
            secs,
        }
    }

    fn since(&self) -> Option<time::OffsetDateTime> {
        time::OffsetDateTime::parse(&self.since, &time::format_description::well_known::Rfc3339)
            .ok()
    }

    /// The instant past which the work is overdue. A `since` that
    /// cannot be parsed counts from `now`: a garbled record earns the
    /// default wait, never an instant check-in.
    fn due(&self, now: time::OffsetDateTime) -> time::OffsetDateTime {
        self.since().unwrap_or(now) + time::Duration::seconds(self.secs as i64)
    }

    fn elapsed(&self, now: time::OffsetDateTime) -> time::Duration {
        now - self.since().unwrap_or(now)
    }

    fn short(&self) -> String {
        short_duration(time::Duration::seconds(self.secs as i64))
    }
}

pub(crate) fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The agent's intent for ONE yield: "I am ending my turn because I
/// am waiting on this, do not nudge me for this yield."
///
/// READ as evidence, and consumed separately from the read. A turn
/// reads it before deciding anything and takes it only if the plan it
/// then forms says to: [`TurnPlan::projection`] keeps the marker
/// when the turn DECLINED the attendance, because declining says this
/// tool cannot deliver the completion wake — the work is still
/// running and the record still describes it. So a marker CAN outlive
/// the turn that read it, and it is removed only by a turn that takes
/// it or by `clank attending --clear`.
///
/// It is not believed on its word either. [`prove_attendance`] yields
/// attendance evidence only for a marker whose recorded process is
/// still the process it recorded — a pid the stored token still holds
/// — because pids are reused and a dead task delivers no wake.
///
/// An unproven marker therefore suppresses NOTHING by itself: it
/// supplies no evidence, and the turn decides exactly as it would
/// with no marker at all. What that is depends on the gates BEFORE
/// it, since [`drive_gate`] runs first — auto off is silent, and an
/// unreadable config, an unresolvable role or a passive adapter is a
/// diagnostic, marker or no marker. Only a turn that is already
/// driving reaches the hook-owned wait.
///
/// What it is NOT is a claim about a DURATION. The earlier design
/// stored the same words as "X is running, stay silent until it
/// stops", which had to remain true over time, so it needed reaping,
/// and the only reaper was the Stop hook the marker silenced. A
/// reviewer sat mute for three hours on that.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attending {
    pub(crate) task: String,
    /// Absent only on records written before it existed; both writers
    /// always set it now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expect: Option<Expectation>,
    /// Two words for what is being waited on. Absent on records
    /// written before it existed, and on callers that omit it — the
    /// task id is the subject then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) desc: Option<String>,
    /// WHO the pid was when it was recorded. A pid alone is reused, so
    /// anything that would signal this process compares the token
    /// first. Absent without `--pid`, on platforms that cannot answer,
    /// and on records written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<crate::proc_identity::ProcToken>,
    /// Absent unless the caller passed `--pid`. A harness task handle
    /// carries no pid, so this arrives only when the backgrounded
    /// command recorded its own `$$`. Display only — nothing about
    /// suppression consults it, since a one-shot marker has no
    /// lifetime to bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pid: Option<i32>,
    /// What kind of live-work evidence may satisfy this marker.
    #[serde(default)]
    pub(crate) correlation: Correlation,
}

impl Attending {
    /// Is there PROVABLY still a wake coming for this attendance?
    ///
    /// Silencing a turn hands the wake channel to something else. For
    /// an attended background task that something is the tool's
    /// completion notification, which will only ever fire while the
    /// task is actually running — so suppressing without proof of
    /// that leaves the agent with no channel at all, which is a
    /// permanent sleep, not a missed nudge.
    ///
    /// Hence proof, not the absence of doubt. `live_ids` alone is not
    /// proof: `a-dead-pid-voids-a-falsely-live-marker` recorded claude
    /// announcing an already-finished task as live (pid gone from the
    /// process table, still listed by the next hook). A bare pid is
    /// not proof either, because pids are reused. Only a pid the
    /// token still holds identifies the process that was attended.
    ///
    /// Anything unverifiable — no pid, no token, legacy record —
    /// answers `None` and keeps its park. Noise, never silence.
    ///
    /// The proof's product is the harness task id the evidence named,
    /// because the check-in that follows must be able to say it: a
    /// `clank run` marker holds only its description (the tool assigns
    /// an id after launching the command), so the id is learned HERE,
    /// from the one live run carrying that description — EXACTLY one;
    /// zero proves nothing and two cannot say which is this marker's
    /// — and a match the tool listed without an id proves nothing
    /// either, since it could not be re-attended (codex on 134f354).
    pub(crate) fn provably_live(&self, input: &HookInput) -> Option<String> {
        // Only the evidence this marker was written for. Accepting
        // either would let each path be satisfied by the other's
        // coincidence.
        let task_id = match self.correlation {
            Correlation::TaskId => input
                .live_task_ids()
                .into_iter()
                .find(|id| id == &self.task)?,
            Correlation::RunDesc => match input.live_clank_runs(&self.task).as_slice() {
                [only] => only.id.clone()?,
                _ => return None,
            },
        };
        self.work_alive().then_some(task_id)
    }

    /// Is the recorded process still the process that was recorded?
    /// `false` for a record that cannot say — no pid, no token.
    pub(crate) fn work_alive(&self) -> bool {
        match (self.pid, self.token.as_ref()) {
            (Some(pid), Some(token)) => pid_is_alive(pid) && token.still_holds(pid),
            _ => false,
        }
    }
}

/// What the Stop hook DECIDED, kept for the human to read.
///
/// The marker is gone within a turn, which is too short to be seen.
/// This is the observation left behind: at `at`, this agent was
/// silenced because it said it was waiting on `task`. It is history
/// plus — via `pid` — a live OS check, never a claim that survived.
///
/// Nothing reads it for suppression. That is precisely what makes it
/// safe to outlive the marker it came from: an inaccurate record
/// misinforms a status line, where an inaccurate marker muted an
/// agent.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attended {
    pub(crate) task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expect: Option<Expectation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pid: Option<i32>,
    /// See [`Attending::token`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<crate::proc_identity::ProcToken>,
    /// RFC3339, for rendering how long ago the decision was made.
    pub(crate) at: String,
}

/// Does this process exist?
///
/// `kill(pid, 0)` runs the existence and permission checks without
/// delivering a signal. `EPERM` means the process EXISTS but belongs
/// to another user — the question here is existence, so that is ALIVE.
///
/// Non-positive pids are rejected rather than passed through: `kill`
/// reads 0 as "every process in my group" and -1 as "every process I
/// may signal", so a corrupt marker holding one must not reach it.
pub(crate) fn pid_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

impl Attended {
    /// Whether the recorded process still exists, or `None` when the
    /// record carries no pid and the question cannot be answered.
    ///
    /// Saying nothing is the correct answer without a pid — never a
    /// guess.
    pub(crate) fn process_alive(&self) -> Option<bool> {
        self.pid.map(pid_is_alive)
    }

    /// How the decision reads on a status surface: the pid a human can
    /// find in `ps` when there is one, the harness handle that names it
    /// to the hook, and either how long ago it was made or the fact
    /// that the process has since ended.
    pub(crate) fn summary(&self, now: time::OffsetDateTime) -> String {
        let mut out = match self.pid {
            Some(pid) => format!("{pid} ({})", self.task),
            None => self.task.clone(),
        };
        match (self.process_alive(), self.age(now)) {
            (Some(false), _) => out.push_str(" · stale, ended"),
            (_, Some(age)) => {
                out.push_str(" · ");
                out.push_str(&age);
            }
            _ => {}
        }
        out
    }

    /// The agent row's form, or `None` to draw no marker at all.
    ///
    /// Narrower than [`Self::summary`] in two ways, because a TUI row
    /// is the scarcest surface there is.
    ///
    /// A wait whose process has ENDED is not rendered. The row answers
    /// "who is waiting, and on what"; a dead process answers "nobody",
    /// so spending columns to say `stale, ended` says nothing. This is
    /// a RENDER decision only — the record is left alone, because
    /// reaping belongs to the hook.
    ///
    /// The harness task id is dropped. It cannot be looked up,
    /// correlated with anything else on screen, or found in `ps`. The
    /// pid is the half a human can act on; the id keeps its place on
    /// the plain `clank status` line, where width is not scarce.
    ///
    /// A record that cannot be CHECKED is not drawn either. Rendering
    /// one is a liveness assertion — "waiting on this, 2m and
    /// counting" — from a record that can never support it, and since
    /// nothing can ever learn the work ended, the row would age
    /// upward forever. That was the reported bug.
    ///
    /// Such records can no longer be written (admission requires a
    /// verifiable identity, and the decision record is only kept for
    /// an attendance that proved itself), so this covers ones already
    /// on disk, until the next hook entry sweeps them.
    /// What the wait IS: the recorded description, else the opaque
    /// task id. Never empty — this is the field the marker exists to
    /// show, and a marker that cannot show it is not drawn at all.
    pub(crate) fn subject(&self) -> &str {
        self.desc
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or(&self.task)
    }

    /// The pid this wait may be signalled at, or `None` when the
    /// process cannot be IDENTIFIED.
    ///
    /// A pid alone is never enough: it is reused, so a recorded number
    /// whose process has exited can come back held by a stranger that
    /// is both alive and killable. The token is what distinguishes
    /// them, and no token means no signal.
    pub(crate) fn killable_pid(&self) -> Option<i32> {
        let pid = self.pid?;
        let token = self.token.as_ref()?;
        token.still_holds(pid).then_some(pid)
    }

    /// Why this wait cannot be killed, phrased for the page. `None`
    /// when it can be. Saying which of the reasons applies is the
    /// point — a control that is simply absent teaches nothing.
    pub(crate) fn kill_blocker(&self) -> Option<&'static str> {
        let Some(pid) = self.pid else {
            return Some("no pid was recorded for this wait, so there is nothing to signal");
        };
        let Some(token) = self.token.as_ref() else {
            return Some(
                "this wait was recorded before clank identified processes, \
                 so the pid cannot be trusted to still be the same process",
            );
        };
        if !pid_is_alive(pid) {
            return Some("that process has already ended");
        }
        (!token.still_holds(pid)).then_some(
            "the pid is in use by a DIFFERENT process now — the recorded one has ended \
             and its number was reused",
        )
    }

    /// The marker's fields in priority order: subject, then age, then
    /// pid. Callers drop from the RIGHT to fit, so a narrowing line
    /// only ever gets shorter. `None` once the process is known dead —
    /// a wait that has ended is not drawn.
    pub(crate) fn marker_fields(&self, now: time::OffsetDateTime) -> Option<MarkerFields<'_>> {
        // Positive evidence, not the absence of bad news: `None` here
        // means "no pid, cannot check", which is not a wait anyone
        // should be shown as ongoing.
        if self.process_alive() != Some(true) {
            return None;
        }
        Some(MarkerFields {
            subject: self.subject(),
            age: self.age(now),
            pid: self.pid,
        })
    }

    /// The WORK's age against its bound — `3m/5m`, and `12m/5m` once
    /// overdue, which says so without a word. A record without an
    /// expectation ages from the decision instead, as it always did.
    pub(crate) fn age(&self, now: time::OffsetDateTime) -> Option<String> {
        let Some(expect) = self.expect.as_ref() else {
            let at = time::OffsetDateTime::parse(
                &self.at,
                &time::format_description::well_known::Rfc3339,
            )
            .ok()?;
            return Some(short_duration(now - at));
        };
        Some(format!(
            "{}/{}",
            short_duration(expect.elapsed(now)),
            expect.short()
        ))
    }
}

/// The attendance marker's parts, most important first. Assembling
/// and fitting them belongs to the renderer, which is where the
/// column budget and the width tables live.
pub(crate) struct MarkerFields<'a> {
    pub(crate) subject: &'a str,
    pub(crate) age: Option<String>,
    pub(crate) pid: Option<i32>,
}

/// The attendance marker's own clock. Seconds are kept here — an
/// expectation is a BUDGET (`45s/5m`), read against a bound that is
/// often minutes, not an age glanced at during a turn.
fn short_duration(d: time::Duration) -> String {
    crate::age::short(d.whole_seconds())
}

/// The last decision, for surfaces that report it. Read-only: status
/// never writes or clears this, because the hook rewrites it on every
/// turn-end anyway.
pub(crate) fn read_attended(agent_dir: &Path) -> Option<Attended> {
    let raw = std::fs::read_to_string(attended_path(agent_dir)).ok()?;
    serde_json::from_str::<Attended>(&raw).ok()
}

/// READ the marker. No effect, so it can be gathered as evidence
/// before anything decides what the turn does.
///
/// Split from the clear because one call that read AND deleted had to
/// sit at an exact point in the control flow — after the yields,
/// which leave the attendance running, and before the auto-off check,
/// which must still take it. That constraint is what stopped the
/// evidence being gathered up front; it was never a property of the
/// turn, only of a function that did two things.
fn read_attending(agent_dir: &Path) -> Option<Attending> {
    // An unreadable marker suppresses nothing — fail toward NOISE,
    // never toward silence.
    std::fs::read_to_string(attending_path(agent_dir))
        .ok()
        .and_then(|r| serde_json::from_str::<Attending>(&r).ok())
}

/// TAKE the marker: this turn owns it and the next must not see it.
///
/// ONE job, and its name is the whole of it. It used to delete the
/// projection too, which the two attended arms then wrote back — so
/// "take the marker" meant two files and half of it was undone a few
/// lines later.
///
/// Best-effort, as the read is. A marker that outlives its turn is
/// how a finished task kept suppressing wakes.
fn clear_attending(agent_dir: &Path) {
    let _ = std::fs::remove_file(attending_path(agent_dir));
}

/// Apply the turn's [`Projection`] to both files. The ONLY writer of
/// either, so what is on disk after a turn is a function of what that
/// turn decided, and not of which arm returned.
///
/// Best-effort throughout: a marker that outlives its turn silences an
/// agent, while a projection that outlives its turn misinforms a
/// status line. Neither is worth failing a hook over.
fn leave_projection(agent_dir: &Path, projection: Projection<'_>) {
    match projection {
        Projection::Untouched => {}
        Projection::Empty => {
            clear_attending(agent_dir);
            let _ = std::fs::remove_file(attended_path(agent_dir));
        }
        Projection::Attended(rec) => {
            clear_attending(agent_dir);
            record_attended(agent_dir, rec);
        }
    }
}

/// Read the marker and leave nothing, for tests with no plan to form.
///
/// Production has no such caller: it reads early, decides, and applies
/// one projection, which is the whole point of the split.
#[cfg(test)]
fn consume_attending(agent_dir: &Path) -> Option<Attending> {
    let rec = read_attending(agent_dir);
    leave_projection(agent_dir, Projection::Empty);
    rec
}

/// The attendance decision, taken at hook ENTRY where `live_ids` is
/// still fresh. `Some` means this turn is silenced — the wake is
/// handed to the attended task's completion, and to the check-in
/// timer the caller then holds; `None` means carry on and park.
///
/// It lives here, and not at wait-return, because the list handed to
/// a returning park was snapshotted before the park began and may be
/// arbitrarily old by then (codex on 81981f2).
/// PURE: the proof, with no trace of having taken it. Recording the
/// projection is [`record_attended`], and the caller runs it — this
/// wrote the file itself, which is the third thing in this file to
/// weld an effect to a read and the third to constrain where it could
/// be called (`consume_attending` 7dfcfd49, `armed` d281959260a2).
fn prove_attendance<'a>(attending: Option<&'a Attending>, input: &HookInput) -> Option<Proven<'a>> {
    let rec = attending?;
    let task_id = rec.provably_live(input)?;
    Some(Proven { rec, task_id })
}

/// A marker the turn's evidence has proven, with what the proof
/// yielded: the harness task id it names. Required, not displayed —
/// the check-in's recipe is built from this, so a marker that could
/// not produce one is never proven.
struct Proven<'a> {
    rec: &'a Attending,
    task_id: String,
}

// ── the check-in (attending-checks-in-when-the-work-runs-long) ──
// A completion wake fires when the work COMPLETES. Work that hangs
// never does, so a silenced turn-end also holds a timer: past the
// expectation, if the work is still running, wake the agent once and
// say so. Nothing is signalled — the bound is on the agent's sleep,
// not the work's life.

/// Names the attendance whose check-in is armed, by its identity
/// (the marker's `since`). A newer attendance overwrites its control
/// file, `--clear`
/// removes it, and an unrelated turn-end leaves it alone — three
/// states the timer can tell apart, where the `attended` projection
/// (swept every entry) could not distinguish a clear from cleanup.
fn holder_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("attending.holder")
}

/// Which holder state the file is in.
///
/// `holder_reads() == false` answered three different questions at
/// once — overwritten, removed, and unreadable — and the timer needs
/// to tell a newer attendance from a cancellation from a failure.
///
/// **Only `NotFound` establishes absence.** Invalid UTF-8, a
/// directory at the path, or a permission error all fail
/// `read_to_string`, and reading any of them as "removed" claims the
/// user cancelled an attendance they never touched — a decision
/// invented from a failure to observe (codex cfb98cbc83a9).
fn holder_state(agent_dir: &Path, identity: &str) -> HolderState {
    match std::fs::read_to_string(holder_path(agent_dir)) {
        Ok(h) if h.trim() == identity => HolderState::Ours,
        Ok(_) => HolderState::Other,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HolderState::Gone,
        Err(e) => HolderState::Unreadable(e.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HolderState {
    Ours,
    Other,
    /// The file is not there. The ONLY state that means cancelled.
    Gone,
    Unreadable(String),
}

/// What `clank attending --clear` found and removed.
pub(crate) struct Cleared {
    pub(crate) marker: bool,
    pub(crate) check_in: bool,
}

/// Withdraw the attendance entirely: the marker for the next turn-end
/// AND the holder, so an armed check-in timer reads nothing and
/// yields. Removing only the marker — which the hook consumed at the
/// turn-end that armed the timer — would cancel nothing (codex on
/// ddb12fc).
pub(crate) fn clear_attendance(agent_dir: &Path) -> std::io::Result<Cleared> {
    let remove = |path: PathBuf| match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    };
    Ok(Cleared {
        marker: remove(attending_path(agent_dir))?,
        check_in: remove(holder_path(agent_dir))?,
    })
}

/// How often the timer re-asks whether it is still worth waking:
/// liveness, holder, and current session binding. Each is a `kill(pid, 0)`, a
/// proc-table read or a small file — nothing spawns.
const CHECK_IN_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Why the check-in timer stopped being worth holding.
///
/// Was one bool (`work_alive() && holder_reads(..)`), and the caller
/// needed the half it did not get: work ending leaves a completion
/// wake in flight — a waker — while a takeover means another timer
/// is attending. Both ended as the same silent label, and a turn that
/// yielded to nobody was indistinguishable from one that yielded to a
/// live task (the-turn-end-is-one-decision).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Disarmed {
    /// The current session no longer holds the label.
    Superseded,
    /// The attended work ended. Its completion wake is already in
    /// flight, and that wake is the waker.
    WorkEnded,
    /// A newer attendance holds the timer. It is attending now.
    TakenOver,
    /// `--clear` removed the holder. The attendance was cancelled,
    /// and nothing is meant to be watching.
    Cleared,
    /// The holder could not be read, so none of the above is known.
    /// Reported, never guessed: a check-in that ends silently on a
    /// failure to observe is the undiagnosable exit this plan exists
    /// to remove.
    Unreadable(String),
}

/// Hold the check-in timer for a PROVEN attendance, and emit its
/// outcome. Arms the holder, then waits for the earliest of: the work
/// ending, the holder changing, the binding changing, the ceiling,
/// or the bound — and only the bound, with the work still live, wakes.
///
/// Liveness and binding checks are injected so the composition can be
/// driven under a paused clock; the holder is read from `agent_dir`.
async fn attend(
    proven: &Proven<'_>,
    agent_dir: &Path,
    ceiling: std::time::Instant,
    mut work_alive: impl FnMut() -> bool,
    still_bound: impl Fn() -> bool,
    // (returns the DECISION: the waker it names is what milestone 4
    // reads, and rendering here would erase it before anyone can.)
) -> TurnDecision {
    let rec = proven.rec;
    // A record without an expectation predates the field; it earns
    // the default from now, and an identity minted now.
    let expect = rec
        .expect
        .clone()
        .unwrap_or_else(|| Expectation::starting_now(DEFAULT_EXPECT_SECS));
    if let Err(e) = std::fs::write(holder_path(agent_dir), &expect.since) {
        return TurnDecision::Failed(format!("hook: arming the check-in: {e}"));
    }

    let now = time::OffsetDateTime::now_utc();
    let remaining: std::time::Duration = (expect.due(now) - now)
        .try_into()
        .unwrap_or(std::time::Duration::ZERO);
    let due = tokio::time::Instant::now() + remaining;

    let mut armed = || {
        if !still_bound() {
            return Some(Disarmed::Superseded);
        }
        if !work_alive() {
            return Some(Disarmed::WorkEnded);
        }
        match holder_state(agent_dir, &expect.since) {
            HolderState::Ours => None,
            HolderState::Other => Some(Disarmed::TakenOver),
            HolderState::Gone => Some(Disarmed::Cleared),
            HolderState::Unreadable(e) => Some(Disarmed::Unreadable(e)),
        }
    };
    let attended = attended_yield(proven);
    let check_in = || {
        check_in_reason(
            rec,
            &expect,
            &proven.task_id,
            time::OffsetDateTime::now_utc(),
        )
    };
    let decision = check_in_when_due(due, ceiling, &mut armed, check_in, attended).await;
    // Wake guard, as the park has: never emit for a dead incarnation.
    if !still_bound() {
        return TurnDecision::NotOurs(NotOurs::Superseded);
    }
    // The holder is left as it is, on purpose. A compare-then-delete
    // has a window in which it deletes a NEWER attendance's holder
    // and silences that timer — the wrong direction. A stale holder
    // costs nothing: only a live timer ever compares against it.
    decision
}

/// The wait itself. `armed` answers "still worth waking?" and is
/// asked before every sleep AND once more when the bound arrives —
/// that last ask is load-bearing: work that ended inside the final
/// poll interval has a completion wake already in flight, and a
/// check-in on top of it would be a wake for nothing.
///
/// The ceiling is not a check-in either, but it is not a silence: it
/// asks once more, and a still-armed timer means the work is live
/// and its completion is the waker.
async fn check_in_when_due(
    due: tokio::time::Instant,
    ceiling: std::time::Instant,
    mut armed: impl FnMut() -> Option<Disarmed>,
    check_in: impl FnOnce() -> String,
    attended: TurnDecision,
) -> TurnDecision {
    let waiting = async {
        loop {
            if let Some(d) = disarm_decision(armed(), &attended) {
                return d;
            }
            let now = tokio::time::Instant::now();
            if now >= due {
                return TurnDecision::Arm(check_in());
            }
            tokio::time::sleep((due - now).min(CHECK_IN_POLL)).await;
        }
    };
    match under_deadline(waiting, ceiling).await {
        Some(d) => d,
        // AT THE CEILING, ASK — do not guess. Still armed means the
        // attended work is live and its completion is what wakes the
        // agent, which is a waker and not a silence; disarmed means
        // the disarm's own answer. This used to share `PollDeadline`
        // with the in-hook wait's expiry, one label over two
        // situations the caller has to tell apart (codex
        // d281959260a2), when the evidence for both is one call away.
        None => disarm_decision(armed(), &attended).unwrap_or(attended),
    }
}

/// What a disarm means, or `None` while the timer is still armed.
///
/// Shared by the poll loop and the ceiling so the two cannot answer
/// the same evidence differently.
fn disarm_decision(disarmed: Option<Disarmed>, attended: &TurnDecision) -> Option<TurnDecision> {
    Some(match disarmed? {
        Disarmed::Superseded => TurnDecision::NotOurs(NotOurs::Superseded),
        // The waker is the task itself, named here because here is
        // where it was last proven live.
        Disarmed::WorkEnded => attended.clone(),
        Disarmed::TakenOver => TurnDecision::NotOurs(NotOurs::AttendanceTakenOver),
        Disarmed::Cleared => TurnDecision::NotOurs(NotOurs::AttendanceCleared),
        Disarmed::Unreadable(e) => {
            TurnDecision::Failed(format!("hook: reading the check-in holder: {e}"))
        }
    })
}

/// The check-in, written for an agent with no context. It must not
/// read as an error or as work: an agent woken with "X has been
/// running 12m" and nothing else will go looking for something to
/// fix. Every value in the recipe is filled in, so it can be pasted.
fn check_in_reason(
    rec: &Attending,
    expect: &Expectation,
    task_id: &str,
    now: time::OffsetDateTime,
) -> String {
    let subject = rec
        .desc
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or(&rec.task);
    // Proven means a pid the token still holds, so this is never the
    // default; it is typed as it is stored.
    let pid = rec.pid.unwrap_or_default();
    let recipe = crate::cli::attending::reattend_command(
        task_id,
        subject,
        pid,
        expect.secs.saturating_mul(2),
    );
    format!(
        "clank: \"{subject}\" (pid {pid}) has been running for {elapsed} — you expected {expected}. \
         It is still running; nothing has been cancelled and nothing has failed. This is a \
         check-in, not a task.\n\nLook at its output so far and decide whether it is stuck. If it \
         just needs longer, re-attend it with a bigger expectation and end your turn:\n\n    \
         {recipe}\n\nOtherwise end your turn; its completion still wakes you when it ends.",
        elapsed = short_duration(expect.elapsed(now)),
        expected = expect.short(),
    )
}

/// Leave the decision behind for `clank status`, since the marker
/// itself is gone within the turn and never visible.
fn record_attended(agent_dir: &Path, rec: &Attending) {
    let Ok(at) =
        time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)
    else {
        return;
    };
    let entry = Attended {
        task: rec.task.clone(),
        expect: rec.expect.clone(),
        desc: rec.desc.clone(),
        pid: rec.pid,
        token: rec.token.clone(),
        at,
    };
    if let Ok(json) = serde_json::to_string(&entry) {
        let _ = std::fs::write(attended_path(agent_dir), json);
    }
}

/// Map the finished wait's output to an outcome. Pure, so the
/// contract can be asserted without spawning anything.
///
/// A superseded wait carries a distinct control envelope even though
/// its process exits nonzero. Other nonzero exits are diagnostics.
fn outcome_from_wait_output(
    output: &std::process::Output,
    label: &AgentLabel,
    role: Role,
    live_ids: &[String],
) -> HookOutcome {
    if matches!(parse_wait_reply(&output.stdout), Ok(WaitReply::Superseded)) {
        return HookOutcome::Silent {
            why: SilentReason::StaleBinding,
        };
    }
    match output.status.code() {
        Some(0) => match parse_wait_reply(&output.stdout) {
            Ok(WaitReply::Superseded) => unreachable!("handled above"),
            Ok(WaitReply::Items(items)) if items.is_empty() => HookOutcome::Diagnostic {
                message: "hook: non-peek wait exited successfully with no items".into(),
            },
            Ok(WaitReply::Items(items)) => {
                // No attendance branch here any more: by the time a
                // park RETURNS, the `live_ids` it was handed are as
                // old as the park itself. Attendance is settled at
                // hook entry, where the evidence is fresh.
                let kept: Vec<&WaitItem> = items.iter().collect();
                let mut reason = render_wait_items_ref(&kept, label, role);
                // A live task and no marker is the shape that produced
                // a dozen identical wakes.
                if !live_ids.is_empty() {
                    reason.push_str(&format!(
                        "\n\nYou have a live background task ({}). To wait on one WITHOUT being \
                         woken, launch it through clank next time — `clank run --desc \"two \
                         words\" -- <command>` — which records what it is attending and then \
                         becomes the command, so clank knows the pid and knows exactly when the \
                         work ends. `clank attending <task-id> --desc \"two words\" --pid <pid>` \
                         does the same job when you already have a pid; without one clank cannot \
                         prove the work is still running and will refuse, because silencing a \
                         turn with no provable wake is how an agent goes to sleep for good. \
                         Either takes `--expect 30m` when the work will outlast the five-minute \
                         default; past that you are checked in on once, and nothing is \
                         cancelled. Do not end your turn to poll it.",
                        live_ids.join(", "),
                    ));
                }
                HookOutcome::Continue { reason }
            }
            Err(e) => HookOutcome::Diagnostic {
                message: format!("hook: wait stdout malformed: {e}"),
            },
        },
        other => HookOutcome::Diagnostic {
            message: format!(
                "hook: wait exited {} stderr={}",
                other
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "(signaled)".into()),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        },
    }
}

fn read_hook_stdin() -> Result<HookInput, String> {
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        return Err(format!("hook: reading stdin: {e}"));
    }
    serde_json::from_str(&buf).map_err(|e| format!("hook: parsing stdin JSON: {e}"))
}

fn resolve_hook_repo(repo_override: Option<&Path>, input: &HookInput) -> Result<PathBuf, String> {
    if let Some(p) = repo_override {
        return resolve_repo(Some(p)).map_err(|e| format!("hook: --repo {p:?} invalid: {e:#}"));
    }
    // Default: hook stdin's cwd. The agent is sitting at this
    // directory when it ends a turn — that IS the repo we want
    // to project against.
    let cwd = Path::new(&input.cwd);
    resolve_repo(Some(cwd)).map_err(|e| {
        format!(
            "hook: cwd `{}` from hook stdin is not inside a git repo: {e:#}",
            input.cwd
        )
    })
}

/// What a turn-end decided, as a value.
///
/// The gauntlet it replaces was a run of `return` statements whose
/// order mattered and was written down nowhere, and whose one
/// load-bearing effect sat in the middle of it. Naming the decision
/// is what lets the invariant be stated at all
/// (the-turn-end-is-one-decision).
///
/// **The invariant:** a turn clank is RESPONSIBLE FOR CONTINUING ends
/// either armed, or yielding to a named waker that is live. The scope
/// clause is not a hedge — `AutoOff` arms nothing on purpose, and
/// demanding a waker there would turn an intentional stop into a bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnDecision {
    /// Clank is not continuing this turn, and that is the configured
    /// or structural intent rather than a hole.
    NotOurs(NotOurs),
    /// We meant to decide and could not. Surfaced, never swallowed:
    /// an undiagnosable quiet exit is what cost a dead session hours.
    Failed(String),
    /// Someone else wakes this agent, and here is who.
    Yield(YieldTo),
    /// The agent is resumed with text it acts on: work handed back,
    /// the check-in, or the legacy adapter's arm instruction — which
    /// is its only continuation, and why that instruction carries the
    /// registration. ONE variant, because the property that matters
    /// is the same either way: the agent has a next step.
    Arm(String),
}

/// Why clank is not continuing this turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotOurs {
    /// The agent asked not to be driven.
    AutoOff,
    /// A newer incarnation holds the label; this one's output does
    /// not count.
    Superseded,
    /// A newer attendance took the check-in timer.
    AttendanceTakenOver,
    /// `clank attending --clear` cancelled the attendance this turn
    /// was holding a timer for.
    AttendanceCleared,
}

/// Who wakes the agent instead — and the evidence that says so.
///
/// Each variant carries what identified the waker, because a waker
/// cannot be reconstructed from a historical label: the same
/// `SilentReason` is written by a path that proved a wait live and by
/// one that proved nothing, and a value that holds no evidence lets
/// the second masquerade as the first (codex, d281959260a2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum YieldTo {
    /// A background task the agent recorded, whose pid the stored
    /// token still holds.
    AttendedTask { task: String, pid: i32 },
}

/// The reasons whose decision is built where the evidence is, and so
/// can never be read back off the label.
///
/// A reason here is NOT unenforced — it is the opposite. Its site
/// constructs the [`TurnDecision`] while it still holds the proof and
/// derives the wire reason from it, which is why
/// [`decision_from_label`] refuses to speak for it.
#[cfg(test)]
const EVIDENCED: &[SilentReason] = &[SilentReason::AttendingBackgroundTask];

/// What a bare [`SilentReason`] can honestly be classified as.
///
/// Sound ONLY for the intentional stops, whose entire content IS the
/// label: `AutoOff` means the agent asked not to be driven and
/// carries nothing else. Everything else returns `None` — a wake
/// cannot be recovered from a historical reason, and the attempt
/// invented a registered wait for a `sleep 60` (codex, d281959260a2).
///
/// Total over [`SilentReason`]: a new one fails to compile here, and
/// its author must either give it an intent or build its decision at
/// the producing site.
#[cfg(test)]
fn decision_from_label(why: SilentReason) -> Option<TurnDecision> {
    match why {
        // Nobody wakes this agent, and that is the answer rather than
        // a hole.
        SilentReason::AutoOff => Some(TurnDecision::NotOurs(NotOurs::AutoOff)),
        SilentReason::StaleBinding => Some(TurnDecision::NotOurs(NotOurs::Superseded)),
        SilentReason::AttendanceCleared => Some(TurnDecision::NotOurs(NotOurs::AttendanceCleared)),
        // Carries a waker, but not one this function may invent: the
        // token lives at the site. See `EVIDENCED`.
        SilentReason::AttendingBackgroundTask => None,
    }
}

/// The turn yielding to an attendance that just proved itself.
///
/// One function because both attended sites — the legacy synchronous
/// hook and the check-in timer — name the same waker, and two spellings
/// of "who wakes this agent" is the duplication this plan is about.
fn attended_yield(proven: &Proven<'_>) -> TurnDecision {
    TurnDecision::Yield(YieldTo::AttendedTask {
        task: proven.task_id.clone(),
        // Proven means a pid the stored `ProcToken` still holds, so
        // this names a process that WAS that process — not merely a
        // number that is currently in use.
        pid: proven.rec.pid.unwrap_or_default(),
    })
}

/// The wire outcome a decision produces.
///
/// This is the direction that keeps its evidence: the decision is
/// built where the proof is and the label falls out of it, so the
/// label never has to be un-lost.
pub(crate) fn outcome_of(decision: &TurnDecision) -> HookOutcome {
    match decision {
        TurnDecision::NotOurs(NotOurs::AutoOff) => HookOutcome::Silent {
            why: SilentReason::AutoOff,
        },
        TurnDecision::NotOurs(NotOurs::Superseded) => HookOutcome::Silent {
            why: SilentReason::StaleBinding,
        },
        TurnDecision::NotOurs(NotOurs::AttendanceTakenOver) => HookOutcome::Silent {
            why: SilentReason::AttendanceCleared,
        },
        TurnDecision::NotOurs(NotOurs::AttendanceCleared) => HookOutcome::Silent {
            why: SilentReason::AttendanceCleared,
        },
        TurnDecision::Yield(YieldTo::AttendedTask { .. }) => HookOutcome::Silent {
            why: SilentReason::AttendingBackgroundTask,
        },
        TurnDecision::Failed(message) => HookOutcome::Diagnostic {
            message: message.clone(),
        },
        TurnDecision::Arm(reason) => HookOutcome::Continue {
            reason: reason.clone(),
        },
    }
}

impl TurnDecision {
    /// The waker this decision names, if it names one.
    ///
    /// `None` for every variant that is not a `Yield` — which is the
    /// point: `NotOurs` names nobody ON PURPOSE.
    #[cfg(test)]
    pub(crate) fn waker(&self) -> Option<&YieldTo> {
        match self {
            TurnDecision::Yield(w) => Some(w),
            _ => None,
        }
    }
}

// ── the wait's wire, and how its items are phrased ──
// Not the turn algorithm: this is the protocol the hook speaks to its
// own `clank wait` child, and the wording an agent is woken with. The
// Stop path uses it at step 5 and SessionStart at its peek.

/// Owned, presence-tolerant mirror of `wait`'s `--json` envelope.
/// The stop hook consumes its OWN subprocess output across a
/// process boundary, so it deserializes into typed rows rather than
/// walking a `serde_json::Value` (typed-json-not-json-macro). Every
/// field is optional and unknown `kind`s fall through to the loose
/// renderer arm: the boundary stays fail-soft — a future `wait` kind
/// or field never breaks parsing.
#[derive(serde::Deserialize)]
struct WaitEnvelope {
    items: Vec<WaitItem>,
    #[serde(default)]
    superseded: bool,
}

enum WaitReply {
    Items(Vec<WaitItem>),
    Superseded,
}

/// One `wait --json` item as the stop hook reads it. `kind` is the
/// `#[serde(tag = "kind")]` discriminant `wait` emits; the remaining
/// fields are the ones [`render_wait_items`] projects into the
/// minimal wake hint (`wait-output-is-a-minimal-hint`). Anything
/// `wait` adds is ignored; anything absent stays `None`.
/// A wire discriminant, normalized AT THE PARSE BOUNDARY.
///
/// Holds the typed value when this build knows the tag, and always
/// the raw text so an unknown one can still be shown. Converting in
/// `Deserialize` rather than in a normalize step is what makes
/// "converted once" true: there is no path that produces a row with
/// the string un-parsed, including the tests' own `from_value`
/// (codex 9cdf153).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tag<T> {
    known: Option<T>,
    raw: String,
}

impl<T> Tag<T> {
    fn known(&self) -> Option<&T> {
        self.known.as_ref()
    }

    /// What to print when nothing typed matched.
    fn raw(&self) -> &str {
        &self.raw
    }
}

/// A discriminant this build may or may not know. `None` is the
/// compatibility fallback and nothing else.
pub(crate) trait FromTag: Sized {
    fn from_tag(s: &str) -> Option<Self>;
}

impl FromTag for clank_core::wait::WaitKind {
    fn from_tag(s: &str) -> Option<Self> {
        clank_core::wait::WaitKind::parse(s)
    }
}

impl FromTag for clank_core::vocab::WaitingReason {
    fn from_tag(s: &str) -> Option<Self> {
        clank_core::vocab::WaitingReason::parse(s)
    }
}

impl<'de, T: FromTag> serde::Deserialize<'de> for Tag<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(Tag {
            known: T::from_tag(&raw),
            raw,
        })
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct WaitItem {
    pub(crate) kind: Option<Tag<clank_core::wait::WaitKind>>,
    pub(crate) plan: Option<String>,
    pub(crate) sha: Option<String>,
    finalized_at: Option<String>,
    gate: Option<String>,
    next: Option<String>,
    pub(crate) reason: Option<Tag<clank_core::vocab::WaitingReason>>,
    prompt: Option<String>,
    name: Option<String>,
    priority: Option<u64>,
    agent: Option<String>,
    answer: Option<String>,
    pr: Option<u64>,
    round: Option<u64>,
    in_progress: Option<String>,
    new_plans: Option<Vec<String>>,
    // External wake payloads (external-wake-hints-carry-payload):
    // github_event / command_event fields, all optional like the rest.
    repo: Option<String>,
    event: Option<String>,
    detail: Option<String>,
    number: Option<u64>,
    title: Option<String>,
    actor: Option<String>,
    url: Option<String>,
    /// The watch's operator prompt (github-watch-prompts).
    instructions: Option<String>,
    /// A command event's payload
    /// (a-command-is-an-event-source-like-any-other). `event` and
    /// `title` above are shared with the github item.
    content: Option<String>,
    failure: Option<bool>,
    /// What `clank events ack` takes for this one.
    selector: Option<String>,
    /// A draft change's payload (the-review-session-watches-its-pr).
    /// `pr` is shared with the pr-review items above.
    comment: Option<u64>,
    path: Option<String>,
    change: Option<String>,
    revision: Option<u64>,
    // An inbox message's payload.
    handle: Option<String>,
    from: Option<String>,
    subject: Option<String>,
}

impl WaitItem {
    /// The wake's kind, already typed by the parse. `None` for a tag
    /// this build has no arm for.
    fn wake_kind(&self) -> Option<clank_core::wait::WaitKind> {
        self.kind.as_ref()?.known().copied()
    }

    /// The wake's reason, already typed by the parse. `None` when
    /// absent OR unknown; `discharge_of` treats both as "no condition
    /// to state" rather than borrowing one.
    fn wake_reason(&self) -> Option<clank_core::vocab::WaitingReason> {
        self.reason.as_ref()?.known().copied()
    }

    /// The kind's raw text, for rendering one this build cannot type.
    fn raw_kind(&self) -> &str {
        self.kind.as_ref().map_or("?", Tag::raw)
    }

    /// The reason's raw text, shown verbatim so an unrecognised one
    /// is still legible rather than becoming `?`.
    fn raw_reason(&self) -> &str {
        self.reason.as_ref().map_or("?", Tag::raw)
    }
}

fn parse_wait_json(raw: &[u8]) -> Result<Vec<WaitItem>, String> {
    match parse_wait_reply(raw)? {
        WaitReply::Items(items) => Ok(items),
        WaitReply::Superseded => Err("unexpected control response in a peek".into()),
    }
}

fn parse_wait_reply(raw: &[u8]) -> Result<WaitReply, String> {
    let envelope: WaitEnvelope =
        serde_json::from_slice(raw).map_err(|e| format!("not valid JSON: {e}"))?;
    if envelope.superseded {
        if !envelope.items.is_empty() {
            return Err("superseded wait also carried work items".into());
        }
        Ok(WaitReply::Superseded)
    } else {
        Ok(WaitReply::Items(envelope.items))
    }
}

/// Render wait's JSON `items` array into the continuation prompt
/// body. The legacy Claude peek also uses it when work is already
/// ready. Loose stringly-typed projection because we're consuming
/// our own JSON output via subprocess. One MINIMAL line per item
/// — who/verb + plan + 12-char sha (`wait-output-is-a-minimal-hint`):
/// the HOW (feedback-write form, verdicts, promote evaluation,
/// unblock) lives in the agent's skill doc, not re-taught per
/// wake.
// ── discharge conditions (wakes-state-their-discharge-condition) ──
//
// A wake says what to do. Without saying what STOPS it, an agent that
// believes it already acted has no way to tell it is looping — every
// wake looks like a fresh instruction. The condition is a pure
// function of the item, so no state is stored and level-triggering is
// untouched.

/// Why a wake will or will not fire again.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Discharge {
    /// Fires while a condition holds; the text says what ends it.
    Until(&'static str),
    /// Watcher delivery, not stop-hook wake delivery.
    ObserverOnly,
    /// This kind, with this reason, has no condition to state.
    ///
    /// A DECISION, not an omission — which is the difference from the
    /// `Option<Discharge>` this replaces. `None` there meant both
    /// "deliberately silent" and "nobody classified it", and the
    /// second is how `command_event`'s text went stale under a guard
    /// that was never called (the-turn-end-is-one-decision).
    Unconditioned,
}

/// Classify one wake. `reason` matters for `master`, whose several
/// `WaitingReason`s discharge differently.
///
/// TOTAL: every kind lands on a variant, so adding a `WaitKind`
/// fails to compile here rather than falling into a catchall. That
/// catchall is how `fix_commit_tag` went unclassified, and the
/// hand-rolled `wire_tag_of` stand-in for this exhaustiveness was
/// never called (the-turn-end-is-one-decision).
pub(crate) fn discharge_of(
    kind: clank_core::wait::WaitKind,
    reason: Option<clank_core::vocab::WaitingReason>,
) -> Discharge {
    use Discharge::*;
    use clank_core::vocab::WaitingReason;
    use clank_core::wait::WaitKind as K;
    match kind {
        K::Master => match reason {
            // Says only what ENDS it. The `clank attending` recipe is
            // the hint's job (attending-suppresses-standing-wakes),
            // which fires only when a background task is actually
            // live — advertising it unconditionally here would tell an
            // agent with nothing running to record nothing.
            Some(WaitingReason::GateContinue) => Until("fires until a commit moves the gate"),
            Some(WaitingReason::AddressCommitChanges) => {
                Until("fires until you commit work addressing the feedback on that sha")
            }
            Some(WaitingReason::ReadyToFinalize) => Until("fires until `clank finish` runs"),
            Some(WaitingReason::CommitPlanRevision) => {
                Until("fires until the plan edit is committed")
            }
            // Master never carries these, and a borrowed condition
            // reads as authoritative — worse than none.
            Some(WaitingReason::SessionFinished | WaitingReason::CommitNeedsReview) | None => {
                Unconditioned
            }
        },
        K::Reviewer => Until("fires until you write feedback for that sha"),
        K::Finished | K::AdHocSettled => {
            Until("fires until the event is acked — `clank events ack`")
        }
        K::FixCommitTag => Until("fires until the commit's plan tag is corrected"),
        K::MultiplePlansOpen => Until(
            "fires until one plan is active — `clank finish` one, or `clank stash push` the rest",
        ),
        K::PromoteFromQueue => Until("fires until the item is promoted or removed from the queue"),
        K::Message => Until("fires until the message is acked — `clank events ack`"),
        K::GithubEvent => Until("fires until the event is acked — `clank events ack`"),
        K::AdHocReview => Until("fires until feedback exists for that sha"),
        K::AdHocRevise => Until("fires until you commit the revision"),
        K::PrReviewer => Until("fires until your review for that round is written"),
        K::PrMaster => Until("fires until you act on that round"),
        K::Unblocked => Until("fires until you acknowledge it — `clank block clean`"),
        K::Blocked => Until(
            "you CANNOT clear this — only the human can answer it. Do not act on it, and do \
             not answer it yourself",
        ),
        K::Idle => Until("fires while there is nothing else to do"),
        K::DraftChanged => Until("read the draft, do not reply to the edit, then ack the event"),
        // Same discharge as a github event, because it is the same
        // WAL: a command source's event is durable, re-presented on
        // every arm, and finished by an ack
        // (a-command-is-an-event-source-like-any-other). It used to
        // say "change or remove the command source" — true when an
        // exiting producer WAS the event, and advice to break the
        // watch now that one isn't.
        K::CommandEvent => Until("fires until the event is acked — `clank events ack`"),
        K::ForCommit | K::ForFinished | K::ForBlocked => ObserverOnly,
    }
}

/// The condition line for one item, or nothing.
///
/// `None` for `kind` is the ONE fail-soft: a tag from a newer binary
/// this build has no arm for renders without a condition rather than
/// breaking the read. Every other absence is now a `Discharge`
/// variant, so "unclassified" cannot be reached by forgetting
/// (the-turn-end-is-one-decision).
fn discharge_suffix(
    kind: Option<clank_core::wait::WaitKind>,
    reason: Option<clank_core::vocab::WaitingReason>,
) -> String {
    let Some(kind) = kind else {
        return String::new();
    };
    match discharge_of(kind, reason) {
        // NOT `↳` — that already marks a watch's standing
        // instructions (`github-watch-prompts`), and two meanings on
        // one glyph is how a reader stops trusting either.
        Discharge::Until(t) => format!("      · {t}\n"),
        Discharge::ObserverOnly | Discharge::Unconditioned => String::new(),
    }
}

/// What READS the feedback a wake is about, on the wake that is about
/// it. The skill has always said `clank feedback read`, and a reader
/// who does not remember it reaches for the files instead — which are
/// right there, and wrong in the same way every time the layout moves
/// (the-nudge-names-the-command).
///
/// Only wakes whose work IS the feedback on a sha, and only their own
/// sha.
fn read_hint(item: &WaitItem) -> String {
    use clank_core::vocab::WaitingReason as R;
    use clank_core::wait::WaitKind as K;
    // TYPED, like every other discriminant branch: a kind spelled
    // here and never emitted would silently stop naming the command
    // (codex bf22b5b).
    let about_feedback = match (item.wake_kind(), item.wake_reason()) {
        (Some(K::Master), Some(R::AddressCommitChanges | R::ReadyToFinalize)) => true,
        // The same need on a commit that belongs to no plan.
        (Some(K::AdHocRevise), _) => true,
        _ => false,
    };
    match (about_feedback, item.sha.as_deref()) {
        (true, Some(sha)) => format!(
            "      · read it: clank feedback read --commit {}\n",
            short_sha(sha)
        ),
        _ => String::new(),
    }
}

fn render_wait_items(items: &[WaitItem], label: &AgentLabel, role: Role) -> String {
    let refs: Vec<&WaitItem> = items.iter().collect();
    render_wait_items_ref(&refs, label, role)
}

fn render_wait_items_ref(items: &[&WaitItem], label: &AgentLabel, role: Role) -> String {
    use clank_core::wait::WaitKind as K;
    let mut out = String::new();
    out.push_str(&format!(
        "Clank wait returned work for `{label}` ({role}). Items:\n",
        label = label.as_str(),
        role = role.as_str(),
    ));
    for item in items {
        let plan = item.plan.as_deref().unwrap_or("?");
        let full = item
            .sha
            .as_deref()
            .or(item.finalized_at.as_deref())
            .unwrap_or("");
        let short = short_sha(full);
        // The typed kind DISPATCHES; the raw tag is kept only to
        // render one this build has no arm for. Branching on the
        // string let a new core kind fall into the catchall and print
        // a generic `? @` hint, which is the stale-renderer failure
        // this milestone exists to remove (codex bf22b5b).
        let Some(kind) = item.wake_kind() else {
            out.push_str(&format!("  - {}: {plan} @ {short}\n", item.raw_kind()));
            out.push_str(&read_hint(item));
            continue;
        };
        // Minimal hints (`wait-output-is-a-minimal-hint`): one line
        // per item — WHO/verb + plan + short sha. The HOW (the
        // `feedback write` form, verdict meanings, promote
        // evaluation, unblock command) lives in the agent's skill
        // doc, not re-taught per wake. The 12-char sha is what the
        // agent composes `feedback write --commit` from; `clank
        // status` has the rest.
        match kind {
            K::Reviewer => out.push_str(&format!("  - reviewer: review {plan} @ {short}\n")),
            K::Master => {
                let next = item.next.as_deref().unwrap_or("?");
                let reason = item.raw_reason();
                out.push_str(&format!("  - master: {next} {plan} @ {short} ({reason})\n"));
            }
            K::Finished => {
                let ack = item
                    .selector
                    .as_deref()
                    .map(|s| format!(" — ack `clank events ack {s}`"))
                    .unwrap_or_default();
                out.push_str(&format!("  - finished: {plan} @ {short}{ack}\n"));
            }
            K::Idle => {
                let prompt = item.prompt.as_deref().unwrap_or("");
                out.push_str(&format!("  - idle: {prompt}\n"));
            }
            K::AdHocReview => out.push_str(&format!("  - adhoc-review: {short}\n")),
            K::AdHocRevise => out.push_str(&format!("  - adhoc-revise: {short}\n")),
            K::AdHocSettled => {
                let gate = item.gate.as_deref().unwrap_or("?");
                let ack = item
                    .selector
                    .as_deref()
                    .map(|s| format!(" — ack `clank events ack {s}`"))
                    .unwrap_or_default();
                out.push_str(&format!("  - adhoc-settled: {short} (gate {gate}){ack}\n"));
            }
            K::Message => {
                let handle = item.handle.as_deref().unwrap_or("?");
                let from = crate::inbox::sender_words(item.from.as_deref(), item.repo.as_deref());
                let subject = item.subject.as_deref().unwrap_or("");
                // The SELECTOR, not the handle: the handle names the
                // message inside its recipient, and `clank events ack`
                // takes neither it nor nothing.
                let ack = item
                    .selector
                    .as_deref()
                    .map(|s| format!(" — ack `clank events ack {s}`"))
                    .unwrap_or_default();
                out.push_str(&format!("  - message: {handle} {from}: {subject}{ack}\n"));
            }
            K::PromoteFromQueue => {
                let name = item.name.as_deref().unwrap_or("?");
                let priority = item.priority.unwrap_or(0);
                out.push_str(&format!("  - promote: {name} (priority {priority:03})\n"));
            }
            K::Blocked => {
                let agent = item.agent.as_deref().unwrap_or("?");
                let name = item.name.as_deref().unwrap_or("?");
                let scope = item.plan.as_deref().unwrap_or("repo");
                out.push_str(&format!(
                    "  - blocked: {agent}/{name} on {scope} (awaiting human)\n"
                ));
            }
            K::Unblocked => {
                let name = item.name.as_deref().unwrap_or("?");
                // The answer is the action payload (the human's
                // instruction) — signal, not tutorial.
                let answer = item.answer.as_deref().unwrap_or("");
                out.push_str(&format!("  - unblocked: {name}: {answer}\n"));
            }
            K::PrReviewer => {
                let pr = item.pr.unwrap_or(0);
                let round = item.round.unwrap_or(0);
                out.push_str(&format!("  - pr-review: review #{pr} round {round}\n"));
            }
            K::PrMaster => {
                let pr = item.pr.unwrap_or(0);
                let round = item.round.unwrap_or(0);
                let next = item.next.as_deref().unwrap_or("?");
                out.push_str(&format!(
                    "  - pr-review: master {next} #{pr} round {round}\n"
                ));
            }
            // The one deliberately non-minimal item: the remedies are
            // parameterized by the plan names (WHICH stash push goes
            // first depends on them), so unlike the static HOW in the
            // skill doc they must ride in the nudge itself
            // (soft-disallow-multiple-plans).
            K::MultiplePlansOpen => {
                let in_progress = item.in_progress.as_deref().unwrap_or("?");
                let news: Vec<&str> = item
                    .new_plans
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|s| s.as_str())
                    .collect();
                out.push_str(&format!(
                    "  - multiple-plans-open: commits exist for {} while `{}` is \
                     still unfinished. Do NOT keep working on either until you \
                     resolve this by ONE of:\n",
                    news.iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    in_progress,
                ));
                for r in crate::cli::wait::multi_plan_open_remedies(in_progress, &news) {
                    out.push_str(&format!("      {r}\n"));
                }
            }
            // External wakes carry their payload — a bare `? @ ?`
            // fallback hint is unactionable
            // (external-wake-hints-carry-payload).
            K::GithubEvent => {
                let repo = item.repo.as_deref().unwrap_or("?");
                let event = item.event.as_deref().unwrap_or("?");
                let detail = item
                    .detail
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default();
                let num = item.number.map(|n| format!(" #{n}")).unwrap_or_default();
                let title = item
                    .title
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .map(|t| format!(" \"{t}\""))
                    .unwrap_or_default();
                let actor = item
                    .actor
                    .as_deref()
                    .map(|a| format!(" by {a}"))
                    .unwrap_or_default();
                let url = item
                    .url
                    .as_deref()
                    .map(|u| format!(" {u}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  - github: {event}{detail} {repo}{num}{title}{actor}{url}\n"
                ));
                // The watch's standing instructions, one sanitized
                // indented follow-on line (github-watch-prompts) —
                // same collapse as the wait's own hint surface.
                if let Some(i) = item
                    .instructions
                    .as_deref()
                    .filter(|i| !i.trim().is_empty())
                {
                    out.push_str(&format!("    ↳ {}\n", crate::cli::wait::one_line(i, 300)));
                }
            }
            K::CommandEvent => {
                let name = item.name.as_deref().unwrap_or("?");
                let event = item.event.as_deref().unwrap_or("?");
                let content = item
                    .content
                    .as_deref()
                    .filter(|c| !c.is_empty())
                    .map(|c| format!(": {c}"))
                    .unwrap_or_default();
                // A source reporting itself broken is not news about
                // the world, and the remedy is not to act on it.
                let lead = if item.failure.unwrap_or(false) {
                    "command source BROKEN"
                } else {
                    "command"
                };
                // Named here, not left to a `clank events list`: the
                // line that says an event is owed should say how to
                // finish it.
                let ack = item
                    .selector
                    .as_deref()
                    .map(|s| format!(" — ack `clank events ack {s}`"))
                    .unwrap_or_default();
                out.push_str(&format!("  - {lead}: {name} {event}{content}{ack}\n"));
            }
            K::DraftChanged => {
                let pr = item.pr.map(|n| format!("#{n}")).unwrap_or("?".into());
                let change = item.change.as_deref().unwrap_or("?");
                // The revision rides along because two edits of ONE
                // comment are two changes: without it the second
                // line reads as a duplicate of the first.
                let r = item.revision.map(|r| format!(" r{r}")).unwrap_or_default();
                let ack = item
                    .selector
                    .as_deref()
                    .map(|s| format!(" — ack `clank events ack {s}`"))
                    .unwrap_or_default();
                match (item.comment, item.path.as_deref()) {
                    (Some(c), Some(p)) => out.push_str(&format!(
                        "  - draft {pr}: {change} {p} (comment {c}){r}{ack}\n"
                    )),
                    // `gone` is about the review, not a comment in it.
                    _ => out.push_str(&format!("  - draft {pr}: {change}{r}{ack}\n")),
                }
            }
            // Its OWN variant, not a `master` reason — `work_for`
            // emits it directly. It rendered through the catchall
            // until now, which is exactly how a recurring wake ends up
            // with no explanation.
            K::FixCommitTag => out.push_str(&format!("  - fix-commit-tag: {plan} @ {short}\n")),
            // Watcher delivery: these reach an observer, not this
            // hint surface. Naming them keeps the match EXHAUSTIVE,
            // which is the point — a new core kind now fails to
            // compile here instead of printing `? @`.
            K::ForCommit | K::ForFinished | K::ForBlocked => {
                out.push_str(&format!("  - {}: {plan} @ {short}\n", kind.as_str()))
            }
        }
        out.push_str(&discharge_suffix(item.wake_kind(), item.wake_reason()));
        out.push_str(&read_hint(item));
    }
    out.push_str("\nAct on these items now.");
    out
}

/// 12 chars, matching wait's hint width: agents compose
/// `feedback write --commit <sha>` from this, and 12 hex chars
/// can't realistically be ambiguous (ruthless 201e498 concern 2).
fn short_sha(s: &str) -> &str {
    &s[..s.len().min(12)]
}

/// Render the outcome to the per-tool wire shape and exit.
/// This is the ONLY place in the binary that calls
/// `process::exit` directly — main()'s normal flow can't be
/// trusted to pick the right code for the hook protocol.
fn emit_and_exit(outcome: HookOutcome, tool: Tool) -> ! {
    match (outcome, tool) {
        (HookOutcome::RenewWait, Tool::OpenCode) => {
            println!("{OPENCODE_RENEW_WAIT_WIRE}");
            std::process::exit(HOOK_OK_EXIT);
        }
        (HookOutcome::RenewWait, _) => {
            eprintln!("hook: renew-wait action reached an unsupported adapter");
            std::process::exit(HOOK_OK_EXIT);
        }
        (HookOutcome::Continue { reason }, Tool::Claude) => {
            eprintln!("{reason}");
            std::process::exit(CLAUDE_CONTINUATION_EXIT);
        }
        (HookOutcome::Continue { reason }, Tool::Codex) => {
            let decision = CodexBlockDecision::from_reason(&reason);
            // serde_json::to_string can't fail for this typed
            // shape, but if it did somehow we'd exit silently
            // rather than fail the agent.
            match serde_json::to_string(&decision) {
                Ok(json) => println!("{json}"),
                Err(_) => {}
            }
            std::process::exit(HOOK_OK_EXIT);
        }
        // Grok hooks are passive — nothing we print can continue the
        // agent, so a Continue degrades to a stderr note + exit 0
        // (unreachable while no grok adapter is installed).
        (HookOutcome::Continue { reason }, Tool::Grok) => {
            eprintln!("{reason}");
            std::process::exit(HOOK_OK_EXIT);
        }
        // opencode wire (settled by the M1 spike): plain text on
        // stdout = a continuation for the plugin to inject; EMPTY
        // stdout (the Silent arm) = stay quiescent. Exit 0 either way.
        (HookOutcome::Continue { reason }, Tool::OpenCode) => {
            println!("{reason}");
            std::process::exit(HOOK_OK_EXIT);
        }
        (HookOutcome::Silent { .. }, _) => {
            std::process::exit(HOOK_OK_EXIT);
        }
        (HookOutcome::Diagnostic { message }, _) => {
            eprintln!("{message}");
            std::process::exit(HOOK_OK_EXIT);
        }
    }
}

/// Plugin-only control wire. It is intercepted before OpenCode's
/// non-empty stdout is treated as a model prompt.
const OPENCODE_RENEW_WAIT_WIRE: &str = r#"{"clank":"renew_wait"}"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment a test brings: nothing. Identity resolution
    /// must see the fixture's own setup and not the shell that
    /// happens to be running `cargo test` — every agent inside clank
    /// exports `CLANK_AGENT`, which outranks everything else.
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

    fn hook_input(session: &str, last_msg: Option<&str>) -> HookInput {
        serde_json::from_value(serde_json::json!({
            "session_id": session,
            "cwd": "/tmp",
            "stop_hook_active": false,
            "last_assistant_message": last_msg,
        }))
        .unwrap()
    }

    fn bind(repo: &Path, label: &str, session: &str) -> AgentLabel {
        bind_tool(repo, label, session, Tool::Claude)
    }

    fn bind_tool(repo: &Path, label: &str, session: &str, tool: Tool) -> AgentLabel {
        let label = AgentLabel::parse(label).unwrap();
        let sid = clank_core::ids::SessionId::parse(session).unwrap();
        crate::agent_store::bind_session_to_agent(repo, &label, tool, &sid).unwrap();
        label
    }

    fn write_attending(dir: &Path, task: &str) {
        write_attending_pid(dir, task, None)
    }

    /// A marker that can PROVE itself: this process, plus the token
    /// that identifies it. The only shape entitled to suppress.
    fn write_provable_attending(dir: &Path, task: &str) -> i32 {
        let pid = std::process::id() as i32;
        let rec = Attending {
            expect: None,
            desc: Some("test run".into()),
            token: crate::proc_identity::token_for(pid),
            task: task.to_string(),
            pid: Some(pid),
            correlation: Correlation::TaskId,
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("attending"), serde_json::to_string(&rec).unwrap()).unwrap();
        pid
    }

    fn write_attending_pid(dir: &Path, task: &str, pid: Option<i32>) {
        let rec = Attending {
            expect: None,
            desc: None,
            token: None,
            task: task.to_string(),
            pid,
            correlation: Correlation::TaskId,
        };
        std::fs::write(dir.join("attending"), serde_json::to_string(&rec).unwrap()).unwrap();
    }

    fn wait_output(items: serde_json::Value) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: serde_json::to_vec(&serde_json::json!({ "items": items })).unwrap(),
            stderr: Vec::new(),
        }
    }

    fn standing_item() -> serde_json::Value {
        serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
                           "next":"continue","reason":"gate_continue","gate":"continued"})
    }

    /// The wait-return path, which no longer knows anything about
    /// attendance. Kept taking `dir` so callers still write markers
    /// and can assert they were consumed at entry.
    fn outcome(dir: &Path, items: serde_json::Value, live: &[String]) -> HookOutcome {
        let _ = consume_attending(dir);
        outcome_from_wait_output(
            &wait_output(items),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            live,
        )
    }

    /// Prove, then record — the hook's two steps, in its order.
    ///
    /// Tests go through this rather than calling `prove_attendance`
    /// alone, so that "nothing was recorded" after a failed proof
    /// still means the recording was SKIPPED rather than never
    /// attempted.
    fn prove_and_record<'a>(
        attending: Option<&'a Attending>,
        turn: &HookInput,
        dir: &Path,
    ) -> Option<Proven<'a>> {
        let proven = prove_attendance(attending, turn)?;
        record_attended(dir, proven.rec);
        Some(proven)
    }

    /// The ENTRY decision, as the hook takes it: consume the marker,
    /// then judge it against a fresh task list.
    fn entry_outcome(dir: &Path, live: &[String]) -> Option<HookOutcome> {
        let attending = consume_attending(dir);
        let ids: Vec<&str> = live.iter().map(String::as_str).collect();
        prove_and_record(attending.as_ref(), &tasks_live(&ids), dir).map(|_| HookOutcome::Silent {
            why: SilentReason::AttendingBackgroundTask,
        })
    }

    /// A turn whose tool reported these background tasks, as
    /// `(id, command)` — an id of `None` is a task the tool listed
    /// without one.
    fn turn_with(tasks: &[(Option<&str>, &str)]) -> HookInput {
        let tasks: Vec<serde_json::Value> = tasks
            .iter()
            .map(|(id, command)| serde_json::json!({"id": id, "command": command}))
            .collect();
        serde_json::from_value(serde_json::json!({
            "session_id": "sess-evidence",
            "cwd": "/tmp",
            "stop_hook_active": false,
            "background_tasks": tasks,
        }))
        .unwrap()
    }

    /// Live tasks by id, none of them a `clank run`.
    fn tasks_live(ids: &[&str]) -> HookInput {
        let tasks: Vec<(Option<&str>, &str)> =
            ids.iter().map(|id| (Some(*id), "sleep 60")).collect();
        turn_with(&tasks)
    }

    /// Live `clank run`s, as `(id, desc)`.
    fn runs_live(runs: &[(Option<&str>, &str)]) -> HookInput {
        let lines: Vec<String> = runs
            .iter()
            .map(|(_, desc)| format!("clank run --desc \"{desc}\" -- cargo test"))
            .collect();
        let tasks: Vec<(Option<&str>, &str)> = runs
            .iter()
            .zip(&lines)
            .map(|((id, _), line)| (*id, line.as_str()))
            .collect();
        turn_with(&tasks)
    }

    fn is_attending_silence(out: &HookOutcome) -> bool {
        matches!(
            out,
            HookOutcome::Silent {
                why: SilentReason::AttendingBackgroundTask
            }
        )
    }

    /// Records on disk predate `desc`. They must keep loading, with
    /// the task id as the subject — an upgrade that silently stopped
    /// showing existing waits would be the reported bug again, by a
    /// different route.
    #[test]
    fn an_attended_record_without_a_description_still_loads_and_names_itself() {
        let raw = r#"{"task":"br9711ewy","at":"2026-08-20T14:51:09Z"}"#;
        let rec: Attended = serde_json::from_str(raw).expect("pre-desc record still parses");
        assert_eq!(rec.desc, None);
        assert_eq!(rec.subject(), "br9711ewy", "the id is the subject");

        // And a blank description is not a description.
        let blank = r#"{"task":"br9711ewy","desc":"   ","at":"2026-08-20T14:51:09Z"}"#;
        let rec: Attended = serde_json::from_str(blank).unwrap();
        assert_eq!(rec.subject(), "br9711ewy", "whitespace never names a wait");

        // A recorded description wins.
        let full = r#"{"task":"br9711ewy","desc":"test run","at":"2026-08-20T14:51:09Z"}"#;
        let rec: Attended = serde_json::from_str(full).unwrap();
        assert_eq!(rec.subject(), "test run");
    }

    #[test]
    fn liveness_is_existence_not_permission() {
        assert!(pid_is_alive(std::process::id() as i32), "our own process");
        // pid 1 always exists and is root-owned, so a non-root test
        // run takes the EPERM branch — which must read as ALIVE, since
        // the question is existence. Running as root takes the rc == 0
        // branch and must agree.
        assert!(pid_is_alive(1), "existing but foreign-owned");
        assert!(!pid_is_alive(i32::MAX), "cannot be a live pid");
    }

    /// `kill` reads 0 as "my whole process group" and -1 as "every
    /// process I may signal". A corrupt record holding one must be
    /// rejected before it reaches the syscall.
    #[test]
    fn non_positive_pids_never_reach_kill() {
        assert!(!pid_is_alive(0));
        assert!(!pid_is_alive(-1));
    }

    /// Suppression may only ever hand off to a wake that PROVABLY
    /// still exists. `live_ids` alone cannot establish that — this
    /// repo recorded claude announcing an already-finished task as
    /// live — and a bare pid cannot either, because pids are reused.
    /// So: task listed live, pid alive, and the token still holding
    /// that pid. Anything short of all three keeps its park.
    #[test]
    fn only_a_provably_live_attendance_may_suppress() {
        let me = std::process::id() as i32;
        let token = crate::proc_identity::token_for(me);
        assert!(token.is_some(), "this platform must answer for its own pid");
        let live = tasks_live(&["task-42"]);

        let full = Attending {
            expect: None,
            task: "task-42".into(),
            desc: None,
            pid: Some(me),
            token: token.clone(),
            correlation: Correlation::TaskId,
        };
        assert_eq!(
            full.provably_live(&live).as_deref(),
            Some("task-42"),
            "all three signals agree, and the proof names the task"
        );

        // Each signal removed in turn — every one is load-bearing.
        assert!(
            full.provably_live(&tasks_live(&[])).is_none(),
            "not listed live: the completion wake has already been spent"
        );
        assert!(
            full.provably_live(&tasks_live(&["other"])).is_none(),
            "a DIFFERENT task being live says nothing about this one"
        );
        assert!(
            Attending {
                pid: None,
                ..full.clone()
            }
            .provably_live(&live)
            .is_none(),
            "no pid — the penlock marker's exact shape — cannot prove anything"
        );
        assert!(
            Attending {
                token: None,
                ..full.clone()
            }
            .provably_live(&live)
            .is_none(),
            "a bare pid is not an identity; pids are reused"
        );
        // A pid that cannot be alive, and a token that no longer holds
        // its pid, are each independently fatal.
        assert!(
            Attending {
                pid: Some(-1),
                ..full.clone()
            }
            .provably_live(&live)
            .is_none(),
            "an impossible pid is not alive"
        );
        assert!(
            Attending {
                token: Some(crate::proc_identity::ProcToken::Unrecognised),
                ..full.clone()
            }
            .provably_live(&live)
            .is_none(),
            "an unparseable token never matches, so it never proves"
        );
    }

    /// The two correlation paths must not satisfy each other. Each
    /// marker is evidence about one specific thing and accepts one
    /// specific kind of proof; without the discriminator a hand-written
    /// task-id marker could be silenced by an unrelated `clank run`
    /// that happened to share its text, and vice versa (codex on
    /// c566eea).
    #[test]
    fn a_marker_is_only_satisfied_by_the_evidence_it_was_written_for() {
        let me = std::process::id() as i32;
        let base = Attending {
            expect: None,
            task: "shared-text".into(),
            desc: Some("shared-text".into()),
            pid: Some(me),
            token: crate::proc_identity::token_for(me),
            correlation: Correlation::TaskId,
        };
        let manual = base.clone();
        let owned = Attending {
            correlation: Correlation::RunDesc,
            ..base
        };
        let as_id = tasks_live(&["shared-text"]);
        let as_run = runs_live(&[(Some("bq1"), "shared-text")]);

        // Each is satisfied by its OWN evidence.
        assert_eq!(
            manual.provably_live(&as_id).as_deref(),
            Some("shared-text"),
            "task id satisfies a manual marker"
        );
        assert_eq!(
            owned.provably_live(&as_run).as_deref(),
            Some("bq1"),
            "a live run satisfies an owned marker, and yields the run's id"
        );

        // And by nothing else, however exactly the text coincides.
        assert!(
            manual.provably_live(&as_run).is_none(),
            "a manual marker must NOT be satisfied by a run that shares its text"
        );
        assert!(
            owned.provably_live(&as_id).is_none(),
            "an owned marker must NOT be satisfied by a task id that shares its text"
        );
    }

    /// A run clank OWNS has no harness id to match on, so the
    /// description is its only handle — and exactly one live match is
    /// required. Zero proves nothing is running; two or more cannot
    /// say which is this marker's, and choosing either would be a
    /// guess about whether an agent may be silenced.
    #[test]
    fn an_owned_run_correlates_by_exactly_one_description_match() {
        let me = std::process::id() as i32;
        let rec = Attending {
            expect: None,
            // What `clank run` writes: the description, because no
            // harness id exists when the marker is written.
            task: "test run".into(),
            desc: Some("test run".into()),
            pid: Some(me),
            token: crate::proc_identity::token_for(me),
            correlation: Correlation::RunDesc,
        };

        let one = runs_live(&[(Some("bq1"), "test run")]);
        assert_eq!(
            rec.provably_live(&one).as_deref(),
            Some("bq1"),
            "one live run with this description is the proof"
        );
        assert!(
            rec.provably_live(&runs_live(&[])).is_none(),
            "no live run: nothing shows the work is still going"
        );
        assert!(
            rec.provably_live(&runs_live(&[
                (Some("bq1"), "test run"),
                (Some("bq2"), "test run")
            ]))
            .is_none(),
            "two live runs sharing a description is an ambiguity, not a proof"
        );
        assert!(
            rec.provably_live(&runs_live(&[(Some("bq1"), "other")]))
                .is_none(),
            "a run with a different description is not this one"
        );
        // A match the tool listed WITHOUT an id could never be
        // re-attended, so the check-in could not name it: not proven,
        // and the turn parks instead (codex on 134f354).
        assert!(
            rec.provably_live(&runs_live(&[(None, "test run")]))
                .is_none(),
            "an id-less match proves nothing"
        );

        // The id path is unaffected — but it belongs to a marker
        // written for it, not to this one.
        assert!(
            Attending {
                correlation: Correlation::TaskId,
                ..rec.clone()
            }
            .provably_live(&tasks_live(&["test run"]))
            .is_some()
        );

        // And correlation never substitutes for identity: an
        // unprovable marker stays unprovable however it correlates.
        assert!(
            Attending {
                token: None,
                ..rec.clone()
            }
            .provably_live(&one)
            .is_none(),
            "correlation is not identity"
        );
    }

    /// The wait-return path no longer decides attendance AT ALL.
    ///
    /// It cannot: the `live_ids` handed to it were snapshotted at hook
    /// entry and then carried through an unbounded park, so by the
    /// time work arrives they may be hours stale — the attended task
    /// finished long ago, its wake already spent (codex on 81981f2).
    /// Whatever the marker or the list says here, the work is
    /// DELIVERED.
    #[test]
    fn the_wait_return_path_never_suppresses_however_stale_its_snapshot() {
        for live in [
            Vec::new(),
            vec!["task-42".to_string()],
            vec!["something-else".to_string()],
        ] {
            for item in [
                standing_item(),
                serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
                     "next":"finalize","reason":"ready_to_finalize","gate":"finished"}),
                serde_json::json!({"kind":"unblocked","name":"q","answer":"yes"}),
            ] {
                let dir = tempfile::tempdir().unwrap();
                write_provable_attending(dir.path(), "task-42");
                let out = outcome(dir.path(), serde_json::json!([item.clone()]), &live);
                assert!(
                    matches!(out, HookOutcome::Continue { .. }),
                    "live={live:?} {item} must be delivered, got {out:?}"
                );
                assert!(
                    !attending_path(dir.path()).exists(),
                    "and the marker is still consumed"
                );
            }
        }
    }

    /// Even a malformed empty result takes the turn's marker before
    /// surfacing the failure; it cannot leave stale attendance behind.
    #[test]
    fn empty_wait_failure_still_consumes_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42");
        let out = outcome(dir.path(), serde_json::json!([]), &[]);
        assert!(matches!(out, HookOutcome::Diagnostic { .. }), "got {out:?}");
        assert!(
            !attending_path(dir.path()).exists(),
            "consumed even though the marker was never consulted"
        );
    }

    #[test]
    fn an_unreadable_marker_fails_toward_noise_and_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(attending_path(dir.path()), "not json").unwrap();
        let out = outcome(dir.path(), serde_json::json!([standing_item()]), &[]);
        assert!(
            matches!(out, HookOutcome::Continue { .. }),
            "a corrupt marker must never silence: {out:?}"
        );
        assert!(
            !attending_path(dir.path()).exists(),
            "and must not be left to fail again next turn"
        );
    }

    /// Older markers still PARSE — serde ignores the extra field —
    /// and are still consumed. What they may no longer do is suppress:
    /// none of them carries the pid-plus-token that proves a wake is
    /// still coming, so each keeps its park. That is the penlock
    /// record's shape, and the case this plan exists for.
    #[test]
    fn markers_of_any_older_shape_are_consumed_but_never_suppress() {
        for raw in [
            r#"{"task":"task-42"}"#,
            r#"{"task":"task-42","pid":4242}"#,
            r#"{"task":"task-42","since":"2026-08-20T10:13:13Z"}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(attending_path(dir.path()), raw).unwrap();
            let out = entry_outcome(dir.path(), &["task-42".to_string()]);
            assert!(out.is_none(), "{raw} must keep its park, got {out:?}");
            assert!(!attending_path(dir.path()).exists(), "{raw} not consumed");
        }
    }

    // ── the decision record ──

    #[test]
    fn silencing_records_the_decision_with_its_pid() {
        let dir = tempfile::tempdir().unwrap();
        let pid = write_provable_attending(dir.path(), "task-42");
        let out = entry_outcome(dir.path(), &["task-42".to_string()]).expect("silenced");
        assert!(is_attending_silence(&out));

        let rec = read_attended(dir.path()).expect("decision recorded");
        assert_eq!(rec.task, "task-42");
        assert_eq!(rec.pid, Some(pid));
        assert!(
            !rec.at.is_empty(),
            "and stamps when, so status can age it: {rec:?}"
        );
    }

    /// A decision must not outlive the wake that supersedes it — and
    /// the marker buys exactly ONE quiet turn-end, so the next entry
    /// with everything else identical does not suppress.
    #[test]
    fn any_other_outcome_clears_the_decision() {
        let dir = tempfile::tempdir().unwrap();
        write_provable_attending(dir.path(), "task-42");
        let live = ["task-42".to_string()];
        assert!(entry_outcome(dir.path(), &live).is_some(), "silenced once");
        assert!(read_attended(dir.path()).is_some(), "recorded");

        assert!(
            entry_outcome(dir.path(), &live).is_none(),
            "the marker was consumed, so this turn parks"
        );
        assert!(
            read_attended(dir.path()).is_none(),
            "the wake erased the decision it superseded"
        );
    }

    #[test]
    fn the_record_on_disk_has_the_field_names_status_reads() {
        let dir = tempfile::tempdir().unwrap();
        let pid = write_provable_attending(dir.path(), "task-42");
        entry_outcome(dir.path(), &["task-42".to_string()]).expect("silenced");
        let raw = std::fs::read_to_string(attended_path(dir.path())).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["task"], "task-42");
        assert_eq!(json["pid"], pid);
        assert!(json["at"].is_string());
    }

    /// A pid-less marker leaves NO decision record at all, because it
    /// never suppressed. That closes the display hole by construction:
    /// the record `clank status` renders as an ongoing wait can now
    /// only come from an attendance that proved itself, so there is no
    /// longer such a thing as a row that ages forever because nothing
    /// can ever learn it ended.
    #[test]
    fn an_unprovable_marker_leaves_no_decision_to_display() {
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42");
        assert!(entry_outcome(dir.path(), &["task-42".to_string()]).is_none());
        assert!(
            read_attended(dir.path()).is_none(),
            "nothing was silenced, so there is nothing to show as attending"
        );
    }

    #[test]
    fn no_record_reads_as_no_attendance() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_attended(dir.path()).is_none());
        std::fs::write(attended_path(dir.path()), "{not json").unwrap();
        assert!(read_attended(dir.path()).is_none(), "corrupt reads as none");
    }

    #[test]
    fn no_marker_plus_a_live_task_carries_the_hint() {
        // Teaches the protocol exactly where it would otherwise loop.
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            &["task-42".to_string()],
        );
        match out {
            HookOutcome::Continue { reason } => {
                assert!(reason.contains("task-42"), "names the live task: {reason}");
                assert!(
                    reason.contains("clank attending"),
                    "gives the recipe: {reason}"
                );
                assert!(
                    reason.contains("--pid"),
                    "and teaches the flag suppression now REQUIRES: {reason}"
                );
            }
            other => panic!("no marker means it still wakes, got {other:?}"),
        }
    }

    #[test]
    fn no_background_task_means_no_hint_and_no_suppression() {
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42");
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            &[],
        );
        match out {
            HookOutcome::Continue { reason } => {
                assert!(
                    !reason.contains("clank attending"),
                    "no task, no hint: {reason}"
                )
            }
            other => panic!("nothing to attend means it wakes, got {other:?}"),
        }
    }

    /// A PROVEN ATTENDANCE IS ATTENDED — and only a hook that
    /// outlives the turn holds its timer.
    ///
    /// Both plans record the projection; the timer is what differs.
    /// Returning the legacy one as a plain decision dropped the
    /// recording, which is a thing only the split made visible.
    #[test]
    fn a_proven_attendance_yields_at_once_or_holds_the_timer() {
        let rec = expected(0, 300);
        let ev = |tool, async_loop| TurnEvidence {
            tool,
            async_loop,
            auto: Ok(AutoMode::On),
            role: Ok(Role::Master),
            proven: Some(Proven {
                rec: &rec,
                task_id: "bq1x2y3z4".into(),
            }),
        };
        assert!(matches!(
            decide(ev(Tool::Claude, true)),
            TurnPlan::Attend(_)
        ));
        assert!(matches!(
            decide(ev(Tool::Claude, false)),
            TurnPlan::AttendedYield(_)
        ));
        // Both take the marker and leave the record: the attendance
        // was proven from it, and the record is all anyone can see.
        for async_loop in [true, false] {
            assert!(matches!(
                decide(ev(Tool::Claude, async_loop)).projection(),
                Projection::Attended(_)
            ));
        }

        // A TOOL WITH NO COMPLETION WAKE attends nothing. Yielding to
        // the task would name a wake nobody delivers, and the marker
        // is just as proven — so the difference is the TOOL, which is
        // why claiming every tool auto-wakes survived a suite that only
        // ever asked claude.
        for tool in [Tool::Codex, Tool::OpenCode] {
            let plan = decide(ev(tool, false));
            assert!(
                matches!(
                    plan,
                    TurnPlan::Drive(_, TurnRoute::Wait, Attendance::Declined)
                ),
                "{tool:?} must route to its own wait, not yield to the task",
            );
            // AND IT KEEPS THE MARKER. Declining is not consuming: the
            // work is still running and the record still describes it.
            // Checking only the route let the marker be deleted — the
            // plan was not a `Yield`, and that flag was what cleared
            // it (grok 36518be9bfd9).
            assert!(
                matches!(plan.projection(), Projection::Untouched),
                "{tool:?} declined the attendance; it must not take its marker",
            );
        }

        // And a turn with NO attendance does own the marker, so the
        // exemption is about declining rather than about driving.
        let plan = decide(TurnEvidence {
            tool: Tool::Codex,
            async_loop: false,
            auto: Ok(AutoMode::On),
            role: Ok(Role::Master),
            proven: None,
        });
        assert!(matches!(plan, TurnPlan::Drive(_, _, Attendance::None)));
        assert!(matches!(plan.projection(), Projection::Empty));
    }

    /// WHAT A TURN LEAVES, as a table — read off the files.
    ///
    /// The plan's `projection()` is only a claim; this drives the one
    /// writer and reads both paths back, because the bug this replaces
    /// was not in a predicate. It was that `attended` was DELETED by
    /// the marker sweep and written back by whichever arm needed it,
    /// so the file said the wrong thing in between and the two call
    /// sites had to agree about when.
    #[test]
    fn what_a_turn_leaves_is_a_function_of_its_plan() {
        let rec = expected(0, 300);
        let seed = |dir: &Path| {
            std::fs::write(
                attending_path(dir),
                serde_json::to_string(&rec).expect("marker"),
            )
            .expect("seed marker");
            record_attended(dir, &rec);
            assert!(attending_path(dir).exists() && attended_path(dir).exists());
        };

        // Attended: the marker is taken, the record is left.
        let d = tempfile::tempdir().unwrap();
        seed(d.path());
        leave_projection(d.path(), Projection::Attended(&rec));
        assert!(!attending_path(d.path()).exists(), "marker must be taken");
        assert_eq!(
            read_attended(d.path()).map(|a| a.task),
            Some(rec.task.clone()),
            "the record is the only thing a human sees of the marker",
        );

        // Empty: both go, so nothing describes an earlier turn-end.
        let d = tempfile::tempdir().unwrap();
        seed(d.path());
        leave_projection(d.path(), Projection::Empty);
        assert!(!attending_path(d.path()).exists(), "marker must be taken");
        assert!(
            !attended_path(d.path()).exists(),
            "a turn with nothing to show must leave nothing",
        );

        // Untouched: a declined attendance is still running, so the
        // marker AND the record it describes both survive.
        let d = tempfile::tempdir().unwrap();
        seed(d.path());
        leave_projection(d.path(), Projection::Untouched);
        assert!(
            attending_path(d.path()).exists(),
            "declining must not forget work that is still running",
        );
        assert!(attended_path(d.path()).exists(), "nor its record");
    }

    /// THE ROUTE, as a table.
    ///
    /// Every tool answers, and only two answers exist: the hook-owned
    /// wait, or nothing can drive this adapter. One route for every
    /// drivable tool is what makes `an_empty_wait_is_diagnostic` true
    /// everywhere — it is the structural claim that test used to make
    /// by counting occurrences of a function name.
    #[test]
    fn the_route_is_a_table() {
        for (tool, want) in [
            (Tool::Claude, TurnRoute::Wait),
            (Tool::Codex, TurnRoute::Wait),
            (Tool::OpenCode, TurnRoute::Wait),
            (Tool::Grok, TurnRoute::Undrivable),
        ] {
            assert_eq!(route(tool), want, "{tool:?} routes elsewhere");
        }
    }

    /// THE LOOP MODE CHOOSES A TIMER, NEVER A ROUTE.
    ///
    /// `--loop asyncrewake` says one thing — this hook can outlive the
    /// turn — and the code must say it once. It decides whether a
    /// proven attendance holds the check-in timer; it does not decide
    /// where a driving turn goes. That used to be two match arms with
    /// identical bodies, which no test could ask about, so a branch
    /// that had stopped branching stayed for a release.
    #[test]
    fn the_loop_mode_changes_the_timer_not_the_route() {
        let route_in = |tool, async_loop| match decide(TurnEvidence {
            tool,
            async_loop,
            auto: Ok(AutoMode::On),
            role: Ok(Role::Master),
            proven: None,
        }) {
            TurnPlan::Drive(_, route, _) => Some(route),
            _ => None,
        };
        for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode, Tool::Grok] {
            assert_eq!(
                route_in(tool, true),
                route_in(tool, false),
                "{tool:?} routes differently per loop mode",
            );
        }
    }

    /// THE DRIVE GATE, as a table.
    ///
    /// Every combination, including the ones a repo fixture cannot
    /// easily reach: the precedence used to be a position in a run of
    /// returns, so the only way to ask "what wins?" was to build a
    /// repo that reached that position.
    ///
    /// The load-bearing row is the last: auto-off with a broken
    /// roster must stay AutoOff. An agent that asked not to be driven
    /// hearing about its roster is noise about work it will not do.
    #[test]
    fn the_drive_gate_is_a_table() {
        let off = Ok(AutoMode::Off);
        let on = Ok(AutoMode::On);
        let bad_cfg: Result<AutoMode, String> = Err("reading config.json".into());
        let master: Result<Role, String> = Ok(Role::Master);
        let bad_role: Result<Role, String> = Err("no roster".into());

        // Driving: the role comes back and nothing is decided here.
        assert_eq!(drive_gate(&on, &master), Ok(Role::Master));

        // A config that will not parse outranks everything below it,
        // including auto-off — the read is what failed, so the mode
        // it would have produced is not known.
        for role in [&master, &bad_role] {
            assert_eq!(
                drive_gate(&bad_cfg, role),
                Err(TurnDecision::Failed("reading config.json".into())),
            );
        }

        // An unresolvable role is a stated failure, never a default.
        assert_eq!(
            drive_gate(&on, &bad_role),
            Err(TurnDecision::Failed("no roster".into())),
        );

        // And auto-off outranks that failure: same broken roster,
        // different answer, because this agent is not being driven.
        for role in [&master, &bad_role] {
            assert_eq!(
                drive_gate(&off, role),
                Err(TurnDecision::NotOurs(NotOurs::AutoOff)),
            );
        }
    }

    /// AN UNREADABLE HOLDER IS NOT A CANCELLATION.
    ///
    /// `read_to_string` fails on invalid UTF-8, on a directory at the
    /// path, and on a permission error. Mapping every `Err` to "gone"
    /// turned all three into "the user ran --clear", so the check-in
    /// ended silently on a failure to OBSERVE — the undiagnosable
    /// exit this plan exists to remove (codex cfb98cbc83a9).
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_holder_is_reported_not_read_as_a_clear() {
        let rec = expected(0, 300);

        // GONE: the holder was removed. This one IS a cancellation.
        let dir = tempfile::tempdir().unwrap();
        let (out, _) = attend_with(
            dir.path(),
            &rec,
            far_ceiling(),
            flag(true),
            {
                let p = holder_path(dir.path());
                move || std::fs::remove_file(&p).unwrap()
            },
            std::time::Duration::from_secs(60),
        )
        .await;
        assert_eq!(
            out,
            HookOutcome::Silent {
                why: SilentReason::AttendanceCleared
            },
        );

        // UNREADABLE, two ways. Each is an EXISTING holder, so
        // nothing was cancelled, and each must say so rather than
        // stop quietly.
        for (name, corrupt) in [
            ("invalid utf-8", 0u8),
            // A directory where the file belongs: a different errno,
            // the same wrong answer before this fix.
            ("a directory", 1u8),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let p = holder_path(dir.path());
            let (out, _) = attend_with(
                dir.path(),
                &rec,
                far_ceiling(),
                flag(true),
                move || {
                    std::fs::remove_file(&p).unwrap();
                    if corrupt == 0 {
                        std::fs::write(&p, [0xff, 0xfe, 0xfd]).unwrap();
                    } else {
                        std::fs::create_dir(&p).unwrap();
                    }
                },
                std::time::Duration::from_secs(60),
            )
            .await;
            let HookOutcome::Diagnostic { message } = &out else {
                panic!("{name}: an unreadable holder must be reported, got {out:?}")
            };
            assert!(
                message.contains("check-in holder"),
                "{name}: the diagnostic must name what could not be read: {message}",
            );
        }
    }

    /// The CHECK-IN yields to the attendance it proved, by name.
    ///
    /// `attended_yield` being right leaves its call site unproven:
    /// a hardcoded empty waker inside `attend` survived the suite,
    /// because every other test reads the wire label and the label is
    /// the same for any `AttendedTask`. This drives the real
    /// `attend` and reads what it decided.
    #[tokio::test(start_paused = true)]
    async fn the_check_in_yields_to_the_attendance_it_proved() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        let pid = rec.pid.expect("the fixture attends a real pid");
        let proven = Proven {
            rec: &rec,
            task_id: "bq1x2y3z4".into(),
        };
        // The work has already ended, so the first poll disarms and
        // the completion wake is the waker.
        let decision = attend(&proven, dir.path(), far_ceiling(), || false, || true).await;
        assert_eq!(
            decision.waker(),
            Some(&YieldTo::AttendedTask {
                task: "bq1x2y3z4".into(),
                pid,
            }),
            "the check-in must name the attendance it was holding",
        );
    }

    /// THE HOLDER OUTLIVES ITS TIMER, ON PURPOSE.
    ///
    /// `attend` arms the holder and never removes it. A
    /// compare-then-delete has a window in which it deletes a NEWER
    /// attendance's holder and silences that timer — the wrong
    /// direction — and a stale holder costs nothing, because only a
    /// live timer ever compares against it.
    ///
    /// Correctness resting on an ABSENCE needs a name, or the tidying
    /// edit that adds the delete reads as an improvement and nothing
    /// fails. This is that name: removal belongs to
    /// `clank attending --clear` alone, which the second half asserts
    /// so the test says who DOES own it rather than only who does not.
    #[tokio::test(start_paused = true)]
    async fn the_armed_holder_is_left_for_the_next_attendance() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        let since = rec
            .expect
            .as_ref()
            .expect("the fixture expects")
            .since
            .clone();
        let proven = Proven {
            rec: &rec,
            task_id: "bq1x2y3z4".into(),
        };
        // The work has already ended, so the timer disarms at once and
        // returns — the ordinary way an attended turn finishes.
        let _ = attend(&proven, dir.path(), far_ceiling(), || false, || true).await;

        assert_eq!(
            std::fs::read_to_string(holder_path(dir.path()))
                .ok()
                .as_deref(),
            Some(since.as_str()),
            "a finished timer must leave its holder, identity intact",
        );

        // And `--clear` is what takes it, so the absence this relies on
        // has exactly one owner.
        let cleared = clear_attendance(dir.path()).expect("clear");
        assert!(cleared.check_in, "--clear reports the holder it removed");
        assert!(
            !holder_path(dir.path()).exists(),
            "--clear owns the removal the hook declines to do",
        );
    }

    /// The attended waker is IDENTIFIED — the task the marker
    /// correlated and the pid its token still holds.
    ///
    /// Without reading those fields back, `pid` is write-only: a
    /// mutant that yielded `pid: 0` survived the whole suite, because
    /// `outcome_of` matches `AttendedTask { .. }` and the wire label
    /// is the same either way. Carrying evidence nothing reads is how
    /// it stops being true (the-turn-end-is-one-decision).
    #[test]
    fn an_attended_yield_names_the_task_and_the_process() {
        let rec = expected(0, 300);
        let pid = rec.pid.expect("the fixture attends a real pid");
        let proven = Proven {
            rec: &rec,
            task_id: "bq1x2y3z4".into(),
        };
        assert_eq!(
            attended_yield(&proven).waker(),
            Some(&YieldTo::AttendedTask {
                task: "bq1x2y3z4".into(),
                pid,
            }),
        );
        // The pid is the RECORD's, not this process's by luck: a
        // fixture that attends its own pid would let `std::process::id()`
        // pass for reading the marker.
        let mut other = rec.clone();
        other.pid = Some(pid + 1);
        let proven = Proven {
            rec: &other,
            task_id: "bq1x2y3z4".into(),
        };
        assert_eq!(
            attended_yield(&proven).waker(),
            Some(&YieldTo::AttendedTask {
                task: "bq1x2y3z4".into(),
                pid: pid + 1,
            }),
        );
    }

    /// Every silence is either an intentional stop or was built with
    /// live waker evidence at its producing site. The two sets are
    /// disjoint and cover the generated `SilentReason::ALL` list.
    #[test]
    fn every_silence_is_intentional_or_evidenced() {
        use SilentReason as S;
        let intentional = [S::AutoOff, S::StaleBinding, S::AttendanceCleared];

        for why in SilentReason::ALL {
            let buckets = [intentional.contains(why), EVIDENCED.contains(why)];
            assert_eq!(
                buckets.iter().filter(|b| **b).count(),
                1,
                "{why:?} must be in EXACTLY one of intentional/evidenced",
            );
        }

        // An intentional stop is the one case a bare label settles:
        // its whole content IS the label.
        for why in intentional {
            let decision = decision_from_label(why).expect("an intentional stop is settled");
            assert!(
                matches!(decision, TurnDecision::NotOurs(_)),
                "{why:?} is an intentional stop",
            );
            // `NotOurs` names nobody ON PURPOSE, which was a claim in
            // `waker`'s doc that nothing asked about: every use of it
            // asserted a `Yield`.
            assert_eq!(
                decision.waker(),
                None,
                "{why:?} stops without naming a waker",
            );
        }

        // Everything else: a label alone must not manufacture a
        // waker. `YieldArmed` included — its site has the token, and
        // this function does not.
        for why in EVIDENCED {
            assert_eq!(
                decision_from_label(*why),
                None,
                "{why:?} must not name a waker from a bare label",
            );
        }
    }

    /// A decision's wire form comes from the decision, and a
    /// `Diagnostic` is a FAILURE rather than a quiet stop.
    ///
    /// The distinction is the one that cost a dead session hours: an
    /// undiagnosable silent exit and a stated failure are not the
    /// same turn.
    #[test]
    fn a_diagnostic_is_a_failure_and_a_continuation_is_an_arm() {
        assert_eq!(
            outcome_of(&TurnDecision::Failed("roster".into())),
            HookOutcome::Diagnostic {
                message: "roster".into()
            },
        );
        assert_eq!(
            outcome_of(&TurnDecision::Arm("run `clank wait`".into())),
            HookOutcome::Continue {
                reason: "run `clank wait`".into()
            },
        );
        assert_eq!(
            outcome_of(&TurnDecision::NotOurs(NotOurs::AutoOff)),
            HookOutcome::Silent {
                why: SilentReason::AutoOff
            },
        );
    }

    /// Every kind's discharge says the right KIND of thing, and the
    /// text is what is checked.
    ///
    /// There is no "is it classified?" case any more: `discharge_of`
    /// is total over `WaitKind`, so a new variant fails to COMPILE
    /// and a runtime assertion would be claiming a compile-time
    /// property. What the old test could not see is exactly what this
    /// does — `Until(_)` passed for three commits while
    /// `command_event` told agents to break their own watch
    /// (the-turn-end-is-one-decision).
    #[test]
    fn a_discharge_says_what_actually_ends_it() {
        use Discharge::*;
        use clank_core::vocab::WaitingReason as R;
        use clank_core::wait::WaitKind as K;

        for r in R::ALL {
            let d = discharge_of(K::Master, Some(*r));
            match r {
                // The four master actually carries each name a
                // different ending.
                R::GateContinue
                | R::AddressCommitChanges
                | R::ReadyToFinalize
                | R::CommitPlanRevision => {
                    assert!(matches!(d, Until(_)), "master/{r:?}: {d:?}");
                }
                // The two it never carries state nothing rather than
                // borrowing a neighbour's condition.
                R::SessionFinished | R::CommitNeedsReview => {
                    assert_eq!(d, Unconditioned, "master/{r:?}");
                }
            }
        }
        assert_eq!(discharge_of(K::Master, None), Unconditioned);

        // An event owed until an ACK names the command that ends it.
        for k in [K::GithubEvent, K::CommandEvent] {
            let Until(t) = discharge_of(k, None) else {
                panic!("{k:?} is owed until acked")
            };
            assert!(
                t.contains("acked") && t.contains("clank events ack"),
                "{k:?} must name the ack: {t}"
            );
        }
        // The one wake where ACTING is wrong, and where an agent is
        // most likely to invent an action to stop the nagging.
        let Until(t) = discharge_of(K::Blocked, None) else {
            panic!("blocked carries guidance")
        };
        assert!(t.contains("CANNOT") && t.contains("human"), "{t}");
        assert!(t.contains("not answer it yourself"), "{t}");

        assert!(matches!(discharge_of(K::DraftChanged, None), Until(_)));
        for k in [K::ForCommit, K::ForFinished, K::ForBlocked] {
            assert_eq!(discharge_of(k, None), ObserverOnly, "{k:?}");
        }
    }

    /// A kind this build has no arm for renders WITHOUT a condition.
    ///
    /// The one fail-soft, and it must not become an excuse: it is
    /// reachable only through the parse, never by forgetting an arm.
    #[test]
    fn an_unknown_kind_states_no_condition() {
        assert_eq!(discharge_suffix(None, None), "");
        assert_eq!(clank_core::wait::WaitKind::parse("some_future_kind"), None);
    }

    #[test]
    fn master_keys_on_the_reason_not_the_kind() {
        use clank_core::vocab::WaitingReason as R;
        use clank_core::wait::WaitKind as K;
        // Four reasons, four different discharges. Keying on the kind
        // alone would print a condition that does not apply — worse
        // than silence, because it reads as authoritative.
        let cont = discharge_of(K::Master, Some(R::GateContinue));
        let revise = discharge_of(K::Master, Some(R::AddressCommitChanges));
        assert_ne!(cont, revise);
        assert!(
            matches!(cont, Discharge::Until(t) if t.contains("gate")),
            "continue is discharged by moving the GATE: {cont:?}"
        );
        assert!(
            matches!(revise, Discharge::Until(t) if t.contains("feedback")),
            "revise is discharged by addressing the FEEDBACK: {revise:?}"
        );
        // A reason this build cannot read is not given a borrowed
        // condition — and it cannot even be SPELLED as a string here
        // any more, which is the point of the type.
        assert_eq!(R::parse("some_new_reason"), None);
    }

    /// The wake that sends a master to feedback names what reads it,
    /// on that wake's own sha. Nothing else does: a reviewer WRITES
    /// feedback, and a continue has none to read
    /// (the-nudge-names-the-command).
    #[test]
    fn the_wakes_about_feedback_name_what_reads_it() {
        let render = |item: serde_json::Value| {
            let items: Vec<WaitItem> = serde_json::from_value(serde_json::json!([item])).unwrap();
            let refs: Vec<&WaitItem> = items.iter().collect();
            render_wait_items_ref(&refs, &AgentLabel::parse("claude").unwrap(), Role::Master)
        };
        let reads = format!("clank feedback read --commit {}", &FULL_SHA[..12]);

        for about_feedback in [
            serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
              "next":"revise","reason":"address_commit_changes","gate":"changes_requested"}),
            serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
              "next":"finalize","reason":"ready_to_finalize","gate":"finished"}),
            serde_json::json!({"kind":"adhoc_revise","sha":FULL_SHA}),
        ] {
            let out = render(about_feedback.clone());
            assert!(out.contains(&reads), "{about_feedback}: {out}");
        }

        for not_about_feedback in [
            serde_json::json!({"kind":"master","plan":"p","sha":FULL_SHA,
              "next":"continue","reason":"gate_continue","gate":"continued"}),
            serde_json::json!({"kind":"reviewer","plan":"p","sha":FULL_SHA}),
        ] {
            let out = render(not_about_feedback.clone());
            assert!(
                !out.contains("feedback read"),
                "{not_about_feedback}: {out}"
            );
        }
    }

    /// An unknown KIND and an unknown REASON both survive the real
    /// parse-and-render path, and neither invents a condition.
    ///
    /// Built from the PRODUCTION SERIALIZER: a core `WaitItem` goes
    /// through `cli::wait::render_json`, and the two future-tag cases
    /// alter only the one field under test. A hand-written `json!`
    /// stays green while the producer's shape drifts away from it —
    /// the same fixture mistake that let `exit_code`/`output_tail`
    /// outlive the fields they named (codex 9cdf153).
    #[test]
    fn an_unknown_kind_or_reason_renders_loose_and_claims_nothing() {
        use clank_core::vocab::WaitingReason;
        use clank_core::wait::MasterNext;

        let label = AgentLabel::parse("codex").unwrap();
        // The CONTROL: a real wake, serialized the way `clank wait`
        // serializes it.
        let real = serde_json::to_value(crate::cli::wait::render_json(
            &clank_core::wait::WaitItem::Master {
                plan: crate::lifecycle::PlanKey::parse("p").unwrap(),
                sha: crate::lifecycle::CommitSha::parse(FULL_SHA).unwrap(),
                next: MasterNext::Commit,
                reason: WaitingReason::GateContinue,
                gate: clank_core::vocab::CommitGateState::Continued,
            },
        ))
        .expect("the production serializer");

        let render = |item: serde_json::Value| {
            let env = serde_json::json!({ "items": [item] });
            let items = parse_wait_json(env.to_string().as_bytes()).expect("fail-soft parse");
            render_wait_items(&items, &label, Role::Reviewer)
        };
        let with = |field: &str, value: &str| {
            let mut v = real.clone();
            v[field] = serde_json::Value::String(value.to_string());
            v
        };

        // A known wake states its condition — the control, without
        // which the two cases below would pass against a renderer
        // that had simply stopped stating any.
        let out = render(real.clone());
        assert!(out.contains("fires until a commit moves the gate"), "{out}");

        // A KIND from a newer binary: rendered by its raw tag, no
        // condition claimed.
        let out = render(with("kind", "some_future_kind"));
        assert!(out.contains("some_future_kind: p @"), "{out}");
        assert!(!out.contains("fires until"), "no borrowed condition: {out}");

        // A REASON from a newer binary, on a kind this build knows:
        // the item still renders, the raw reason is still legible,
        // and still no condition.
        let out = render(with("reason", "some_new_reason"));
        assert!(out.contains("master:"), "the item renders: {out}");
        assert!(
            out.contains("some_new_reason"),
            "raw reason stays visible: {out}"
        );
        assert!(!out.contains("fires until"), "no borrowed condition: {out}");
    }

    #[test]
    fn the_rendered_wake_carries_the_condition_and_still_carries_the_sha() {
        let items: Vec<WaitItem> = serde_json::from_value(serde_json::json!([
            {"kind":"master","plan":"p","sha":FULL_SHA,
             "next":"continue","reason":"gate_continue","gate":"continued"}
        ]))
        .unwrap();
        let refs: Vec<&WaitItem> = items.iter().collect();
        let out = render_wait_items_ref(&refs, &AgentLabel::parse("claude").unwrap(), Role::Master);
        assert!(out.contains("gate_continue"), "{out}");
        assert!(
            out.contains("fires until"),
            "the wake says what stops it: {out}"
        );
        assert!(
            out.contains(&FULL_SHA[..12]),
            "the 12-char sha stays — it is what `feedback write --commit` is built from: {out}"
        );
    }

    /// The ceiling is the largest value that does not WRAP.
    ///
    /// This test used to assert the opposite — that the ceiling was
    /// unreachable by a running machine, `> 50 years` — and that
    /// aspiration IS what produced the bug. The field is in seconds
    /// and the timer's cap is in milliseconds, so `i32::MAX` seconds
    /// satisfied a 32-bit intuition while being a thousandfold past
    /// the limit: it wrapped, fired on the next tick, and cancelled
    /// every parked hook sub-second. The assertion stayed green
    /// throughout, because it was checking the ambition rather than
    /// the arithmetic.
    ///
    /// So assert in MILLISECONDS, where the limit actually lives and
    /// where seconds-based reasoning went wrong.
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
        // And genuinely the LARGEST: one more second overflows. Pinned
        // from both sides so the value cannot quietly drift down.
        assert!(
            (secs + 1) * 1_000 > MAX_TIMER_MS,
            "a higher value still fits, so this is not the maximum"
        );

        // The arithmetic the hook performs must stay sound at it.
        let started = std::time::Instant::now();
        assert!(
            poll_deadline(started) > started,
            "the derived deadline is still in the future"
        );
    }

    #[test]
    fn the_poll_deadline_sits_under_the_runner_ceiling() {
        // Asserted as a RELATIONSHIP, not a literal: the deadline is
        // derived from the ceiling clank itself writes into the hook
        // config, so raising one cannot silently outrun the other.
        let started = std::time::Instant::now();
        let ceiling = std::time::Duration::from_secs(crate::cli::setup::HOOK_TIMEOUT_SECS);
        let deadline = poll_deadline(started);
        assert!(
            deadline < started + ceiling,
            "the poll must end BEFORE the runner kills it"
        );
        assert_eq!(
            (started + ceiling) - deadline,
            std::time::Duration::from_secs(POLL_MARGIN_SECS),
            "the gap is exactly the margin — no independent constant"
        );
    }

    #[test]
    fn the_deadline_is_absolute_so_later_arming_gets_less_time() {
        // The bug this shape prevents: computing a relative budget and
        // arming it after the child spawn silently excludes the spawn.
        // Anchored to hook entry, the instant does not move, so time
        // spent setting up genuinely shortens the wait.
        let started = std::time::Instant::now();
        let deadline = poll_deadline(started);
        let after_setup = poll_deadline(started); // same anchor, later call
        assert_eq!(deadline, after_setup, "the deadline must not slide");
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_returns_none_and_drops_the_waiter() {
        // Drives the REAL timeout path. The drop is load-bearing: it is
        // what closes the owner pipe so the `clank wait` child reaps
        // itself instead of being orphaned by our expiry.
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
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "expiry must DROP the waiter — that is what reaps the child"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn work_arriving_before_the_deadline_wins() {
        // The guard against "fixing" the banner by never waiting.
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

    /// A wait that never resolves, driven through the PRODUCTION
    /// composition rather than reassembled from its parts.
    ///
    /// The earlier version of this test called `under_deadline` and
    /// `deadline_outcome` itself, which meant either production exit
    /// could regress to silence with the test still green (codex on
    /// 12d2e29). Going through `await_wait_under_deadline` is the
    /// whole point: that function is where both exits live.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_re_arms_without_spinning_opencode() {
        let arm = || Ok(std::future::pending::<std::io::Result<std::process::Output>>());

        for tool in [Tool::Claude, Tool::Codex] {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let got = await_wait_under_deadline(deadline, deadline_action(tool), arm).await;
            assert!(
                matches!(got, Err(HookOutcome::Continue { .. })),
                "{tool:?} must re-arm at the deadline, got {got:?}"
            );
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let got = await_wait_under_deadline(deadline, deadline_action(Tool::OpenCode), arm).await;
        assert_eq!(got, Err(HookOutcome::RenewWait));
    }

    /// The pre-check exit: the deadline was already gone before
    /// anything was armed. Rarer than the expiry and strictly worse —
    /// it dies having done no waiting at all.
    #[tokio::test(start_paused = true)]
    async fn the_pre_check_exit_re_arms_without_arming_a_wait() {
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);

        let armed = std::cell::Cell::new(false);
        let got = await_wait_under_deadline(past, DeadlineAction::ReArm, || {
            armed.set(true);
            Ok(std::future::pending::<std::io::Result<std::process::Output>>())
        })
        .await;
        assert!(
            matches!(got, Err(HookOutcome::Continue { .. })),
            "the pre-check must re-arm too, got {got:?}"
        );
        assert!(
            !armed.get(),
            "it must short-circuit BEFORE arming — spawning a wait it cannot await leaks one"
        );

        let got = await_wait_under_deadline(past, DeadlineAction::RenewWait, || {
            Ok(std::future::pending::<std::io::Result<std::process::Output>>())
        })
        .await;
        assert!(
            matches!(got, Err(HookOutcome::RenewWait)),
            "the plugin must renew even when the deadline passed before arming"
        );
    }

    /// The heartbeat must never preempt real work.
    #[tokio::test(start_paused = true)]
    async fn work_arriving_before_the_deadline_survives_the_seam() {
        use std::os::unix::process::ExitStatusExt;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        let got = await_wait_under_deadline(deadline, DeadlineAction::ReArm, || {
            Ok(async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: b"items".to_vec(),
                    stderr: Vec::new(),
                })
            })
        })
        .await;
        match got {
            Ok(o) => assert_eq!(o.stdout, b"items"),
            Err(outcome) => panic!("work must win over the heartbeat, got {outcome:?}"),
        }
    }

    /// An agent that re-arms once and then sleeps is still gone, just
    /// later — so renewal has to hold across periods.
    #[tokio::test(start_paused = true)]
    async fn consecutive_expiries_each_re_arm() {
        for period in 0..2 {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let got = await_wait_under_deadline(deadline, DeadlineAction::ReArm, || {
                Ok(std::future::pending::<std::io::Result<std::process::Output>>())
            })
            .await;
            assert!(
                matches!(got, Err(HookOutcome::Continue { .. })),
                "period {period} must re-arm, got {got:?}"
            );
        }
    }

    /// The heartbeat's WORDING is load-bearing, so it gets its own
    /// regression: an agent woken with no items and no explanation
    /// will look for a reason and usually invent one.
    ///
    /// Asserted on the reason the SEAM produces rather than on the
    /// constant — a correct constant is worth nothing if the deadline
    /// path stops using it.
    #[tokio::test(start_paused = true)]
    async fn the_re_arm_reason_cannot_be_mistaken_for_work() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let got = await_wait_under_deadline(deadline, DeadlineAction::ReArm, || {
            Ok(std::future::pending::<std::io::Result<std::process::Output>>())
        })
        .await;
        let Err(HookOutcome::Continue { reason }) = got else {
            panic!("the deadline must re-arm, got {got:?}");
        };
        let r = reason.to_lowercase();
        assert!(
            r.contains("no work"),
            "must say nothing is pending: {reason}"
        );
        assert!(r.contains("re-arm"), "must name itself a re-arm: {reason}");
        assert!(
            r.contains("end your turn"),
            "must say that ending the turn is the correct response: {reason}"
        );
    }

    /// An impossible empty success is a failure, for every adapter.
    ///
    /// The normal wait has no successful empty exit: `--peek` returns
    /// empty, but no adapter passes it. There is ONE driving path, so
    /// surfacing it here surfaces it everywhere — which
    /// `the_route_is_a_table` is what proves, rather than the count of
    /// `compute_wait_outcome(` occurrences this test used to assert.
    /// That count pinned a forwarding wrapper whose body was the same
    /// call, so the duplication was the thing being protected.
    #[test]
    fn an_empty_wait_is_diagnostic() {
        use std::os::unix::process::ExitStatusExt;
        let label = AgentLabel::parse("rev").unwrap();
        let empty = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: br#"{"items":[]}"#.to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(
            outcome_from_wait_output(&empty, &label, Role::Master, &[]),
            HookOutcome::Diagnostic {
                message: "hook: non-peek wait exited successfully with no items".into()
            },
        );
    }

    #[test]
    fn a_superseded_wait_is_not_an_empty_success() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: br#"{"items":[],"superseded":true}"#.to_vec(),
            stderr: b"wait for grok cannot continue: session binding changed".to_vec(),
        };
        let label = AgentLabel::parse("claude").unwrap();
        assert_eq!(
            outcome_from_wait_output(&output, &label, Role::Master, &[]),
            HookOutcome::Silent {
                why: SilentReason::StaleBinding
            }
        );
        assert!(
            parse_wait_json(&output.stdout).is_err(),
            "a peek cannot inherit that control result"
        );
    }

    #[test]
    fn listed_background_wait_does_not_take_stops_wait() {
        let mut input = hook_input("session-16", None);
        input
            .background_tasks
            .push(clank_core::hook_io::BackgroundTask {
                id: Some("task-1".into()),
                status: Some("completed".into()),
                command: Some("clank wait --author codex".into()),
            });
        assert!(matches!(
            decide(TurnEvidence {
                tool: Tool::Codex,
                async_loop: false,
                auto: Ok(AutoMode::On),
                role: Ok(Role::Master),
                proven: prove_attendance(None, &input),
            }),
            TurnPlan::Drive(Role::Master, TurnRoute::Wait, Attendance::None)
        ));
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

    /// A label that another agent held is cleared off it by the bind:
    /// one session, one label.
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

    /// No label in the environment: a session clank did not launch.
    /// It is identified by its id as `clank as` bound it, and an
    /// unbound id binds nothing — today's quiet no-op.
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
                .is_empty(),
            "nothing bound"
        );
        bind(repo, "claude", sid);
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
        // The catch-up wire: a resumed session receives pending work
        // through SessionStart's additionalContext, exactly shaped
        // for claude's hookSpecificOutput schema.
        let v = session_start_payload("Clank wait returned work for `x`.");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.starts_with("Pending clank work found at session start."));
        assert!(ctx.contains("Clank wait returned work"), "{ctx}");
    }

    #[test]
    fn asyncrewake_flag_gates_only_claude() {
        // The mode is argv-keyed (one durable setup-time decision):
        // --loop asyncrewake on claude selects the park; other tools
        // ignore it (setup never writes it for them).
        use crate::cli::LoopModeArg;
        let on = |tool: Tool, lm: Option<LoopModeArg>| {
            tool == Tool::Claude && lm == Some(LoopModeArg::Asyncrewake)
        };
        assert!(on(Tool::Claude, Some(LoopModeArg::Asyncrewake)));
        assert!(!on(Tool::Claude, None));
        assert!(!on(Tool::Codex, Some(LoopModeArg::Asyncrewake)));
    }

    #[tokio::test]
    async fn opencode_binding_resolves_and_idle_routes_a_nudge() {
        // opencode-agent-tool M0: a ses_-shaped binding persists,
        // resolves the label from the session id, and the idle
        // outcome routes the claude-style nudge the plugin relays
        // (final wire shape is the M1 spike's).
        let dir = init_repo();
        let repo = dir.path();
        let label = bind_tool(repo, "kimi", "ses_0af1b2c3d4e5f607", Tool::OpenCode);
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::On).unwrap();
        crate::cli::agent::add_repo_roster_agent(
            repo,
            &label,
            crate::cli::teams_config::AgentDescription {
                tool: Tool::OpenCode,
                launch: None,
                initial_prompt: None,
            },
            crate::cli::teams_config::RosterRole::Commit,
        )
        .unwrap();
        crate::cli::agent::set_repo_master(repo, &label).unwrap();
        std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();

        // The persisted binding round-trips: tool + ses_ id.
        let cfg = crate::agent_store::load_agent_config(repo, &label)
            .unwrap()
            .unwrap();
        let sess = cfg.session.expect("binding persisted");
        assert_eq!(sess.tool, Tool::OpenCode);
        assert_eq!(sess.id.as_str(), "ses_0af1b2c3d4e5f607");

        // Routing is pinned at the PURE boundary — no spawning wait
        // path in unit tests (codex fbc5da1): opencode shares codex's
        // in-hook long-poll policy, whose no-work outcome is silence,
        // never the claude arming nudge (codex 8000d6e: a non-empty
        // relay would loop an opencode session forever). The live
        // spike proved the empty/work/quiescent wire with the real
        // binary.
        assert_eq!(loop_policy(Tool::OpenCode), loop_policy(Tool::Codex));
        assert_eq!(loop_policy(Tool::OpenCode), LoopPolicy::InHookWait);
        assert_ne!(loop_policy(Tool::OpenCode), loop_policy(Tool::Claude));
    }

    #[tokio::test]
    async fn auto_off_is_silent_with_the_branch_reason() {
        let dir = init_repo();
        let repo = dir.path();
        let label = bind(repo, "codex", "sess-auto-off");
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::Off).unwrap();

        let input = hook_input("sess-auto-off", Some("I finished the thing."));
        let outcome = compute_outcome(Tool::Claude, Some(repo), input).await;
        assert_eq!(
            outcome,
            HookOutcome::Silent {
                why: SilentReason::AutoOff
            }
        );
    }

    /// The second early return. `AutoMode::Off` exits above everything
    /// clank does, so it is the same shape as the empty-items route —
    /// "the hook ran but never reached the marker" — which is the
    /// entire bug class this model deletes. Consumption sits before
    /// the auto-mode branch precisely so neither can skip it.
    /// The laundering that wedged a real session: a repo whose role
    /// cannot be resolved used to arm a wait as Reviewer, and for an
    /// agent that is master that poll can never return.
    #[tokio::test]
    async fn unresolvable_role_arms_nothing_and_says_why() {
        let dir = init_repo();
        let repo = dir.path();
        // Bound, auto on, but NO master designated — `resolve_role`
        // has no answer to give.
        let label = bind(repo, "codex", "sess-no-master");
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::On).unwrap();

        let input = hook_input("sess-no-master", Some("done"));
        match compute_outcome(Tool::Claude, Some(repo), input).await {
            HookOutcome::Diagnostic { message } => {
                assert!(message.contains("role"), "names the failure: {message}");
                assert!(
                    message.contains("arming no wait"),
                    "says what it did NOT do: {message}"
                );
            }
            other => panic!("a role we cannot resolve must arm nothing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn swapped_out_auto_identity_is_diagnostic_and_arms_nothing() {
        let dir = init_repo();
        let repo = dir.path();
        let label = bind(repo, "codex", "sess-swapped-out");
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::On).unwrap();
        crate::cli::agent::add_repo_roster_agent(
            repo,
            &label,
            crate::cli::teams_config::AgentDescription {
                tool: Tool::Codex,
                launch: None,
                initial_prompt: None,
            },
            crate::cli::teams_config::RosterRole::Commit,
        )
        .unwrap();
        crate::cli::agent::set_repo_master(repo, &label).unwrap();

        let home = tempfile::tempdir().unwrap();
        let incoming = AgentLabel::parse("scout").unwrap();
        crate::cli::agent::declare_global_agent(
            home.path(),
            &incoming,
            crate::cli::teams_config::AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
        )
        .unwrap();
        crate::cli::agent::swap_repo_agent(repo, Some(home.path()), &label, &incoming).unwrap();

        let input = hook_input("sess-swapped-out", Some("done"));
        match compute_outcome(Tool::Claude, Some(repo), input).await {
            HookOutcome::Diagnostic { message } => {
                assert!(message.contains("not on this repo's roster"), "{message}");
                assert!(message.contains("arming no wait"), "{message}");
            }
            other => panic!("a swapped-out identity must arm nothing, got {other:?}"),
        }
    }

    /// Auto off is the agent asking not to be driven. Diagnosing its
    /// repo's roster would be noise about work it will not do, so the
    /// resolution must sit INSIDE the driving arm, never above it.
    #[tokio::test]
    async fn auto_off_outranks_an_unresolvable_role() {
        let dir = init_repo();
        let repo = dir.path();
        let label = bind(repo, "codex", "sess-off-no-master");
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::Off).unwrap();

        let input = hook_input("sess-off-no-master", Some("done"));
        assert_eq!(
            compute_outcome(Tool::Claude, Some(repo), input).await,
            HookOutcome::Silent {
                why: SilentReason::AutoOff
            },
        );
    }

    /// The seam carries what a test SAYS, not just the absence of a
    /// shell. `CLANK_AGENT` outranks the session binding — which is
    /// the production behaviour, and is precisely why a developer's
    /// exported copy of it used to decide four tests.
    #[tokio::test]
    async fn a_stated_override_cannot_emit_for_an_unbound_label() {
        let dir = init_repo();
        let repo = dir.path();
        let label = bind(repo, "codex", "sess-stated-override");
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::Off).unwrap();

        let stated_elsewhere =
            crate::agent_env::Stated::saying(&[("CLANK_AGENT", "somebody-else")]);
        let outcome = compute_outcome_in(
            Tool::Claude,
            Some(repo),
            hook_input("sess-stated-override", Some("done")),
            &stated_elsewhere,
        )
        .await;
        match outcome {
            HookOutcome::Diagnostic { message } => assert!(
                message.contains("somebody-else"),
                "the stated override decided the identity, not the binding: {message}"
            ),
            HookOutcome::Silent {
                why: SilentReason::StaleBinding,
            } => {}
            other => panic!("a label that is not bound cannot emit: {other:?}"),
        }

        assert_eq!(
            compute_outcome(
                Tool::Claude,
                Some(repo),
                hook_input("sess-stated-override", Some("done"))
            )
            .await,
            HookOutcome::Silent {
                why: SilentReason::AutoOff
            },
            "and with nothing stated, the binding decides"
        );
    }

    #[tokio::test]
    async fn auto_off_still_consumes_the_marker() {
        let dir = init_repo();
        let repo = dir.path();
        let label = bind(repo, "codex", "sess-auto-off-marker");
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::Off).unwrap();

        let agent_dir = crate::agent_store::agents_root(repo).join(label.as_str());
        std::fs::create_dir_all(&agent_dir).unwrap();
        write_attending(&agent_dir, "task-42");

        let input = hook_input("sess-auto-off-marker", Some("done"));
        let outcome = compute_outcome(Tool::Claude, Some(repo), input).await;
        assert_eq!(
            outcome,
            HookOutcome::Silent {
                why: SilentReason::AutoOff
            }
        );
        assert!(
            !attending_path(&agent_dir).exists(),
            "an auto-off turn-end must still take the marker with it"
        );
    }

    #[tokio::test]
    async fn unresolved_identity_is_a_diagnostic() {
        // A session with NO binding: the hook fired but can't tell who it
        // is — a Diagnostic (exit 0 + stderr), never a failure.
        let dir = init_repo();
        let repo = dir.path();

        let input = hook_input("sess-unbound", None);
        let outcome = compute_outcome(Tool::Claude, Some(repo), input).await;
        assert!(matches!(outcome, HookOutcome::Diagnostic { .. }));
    }

    #[test]
    fn a_draft_change_says_what_moved_and_how_to_ack_it() {
        // Built from wait's ACTUAL --json field names, the same wire
        // the hook consumes.
        let raw = serde_json::json!({
            "items": [{
                "kind": "draft_changed",
                "pr": 12,
                "comment": 4088032661u64,
                "path": "crates/cli/src/lib.rs",
                "change": "edited",
                "revision": 3,
                "selector": "draft-pr-12-run-1:3"
            }]
        });
        let items = parse_wait_json(raw.to_string().as_bytes()).unwrap();
        let label = AgentLabel::parse("claude").unwrap();
        let out = render_wait_items(&items, &label, Role::Master);
        assert!(
            out.contains("  - draft #12: edited crates/cli/src/lib.rs (comment 4088032661) r3"),
            "the hint says what moved rather than `? @ ?`: {out}"
        );
        assert!(
            out.contains("read the draft"),
            "and what to do with it: {out}"
        );
        assert!(
            out.contains("do not reply to the edit")
                && out.contains("clank events ack draft-pr-12-run-1:3"),
            "the draft is read, then explicitly acked: {out}"
        );
        assert!(
            out.contains("then ack the event"),
            "the debt remains until ack: {out}"
        );
    }

    #[test]
    fn lifecycle_events_name_their_ack_and_repeat_condition() {
        let out = items_text(&[
            serde_json::json!({"kind":"finished", "plan":"done", "finalized_at":FULL_SHA,
                "selector":"repo-lifecycle@1"}),
            serde_json::json!({"kind":"adhoc_settled", "sha":FULL_SHA, "gate":"continued",
                "selector":"repo-lifecycle@2"}),
        ]);
        assert!(out.contains("finished: done"), "{out}");
        assert!(out.contains("adhoc-settled:"), "{out}");
        assert!(out.contains("clank events ack repo-lifecycle@1"), "{out}");
        assert!(out.contains("clank events ack repo-lifecycle@2"), "{out}");
        assert_eq!(
            out.matches("fires until the event is acked").count(),
            2,
            "{out}"
        );
    }

    /// A source reporting ITSELF broken does not read as news about
    /// the world.
    ///
    /// The agent's remedy is different — the watch is not working, so
    /// there is nothing out there to act on — and an item that reads
    /// like an ordinary event invites acting on it.
    #[test]
    fn a_broken_source_does_not_read_as_something_it_observed() {
        let wire = |failure| {
            serde_json::json!({
                "items": [serde_json::to_value(crate::cli::wait::render_json(
                    &clank_core::wait::WaitItem::CommandEvent {
                        name: "deploys".into(),
                        id: "x".into(),
                        event: "e".into(),
                        content: "`sh -c ...` has stopped 3 times in a row".into(),
                        at: None,
                        failure,
                        selector: Some("command-deploys-0000abcd@2".into()),
                    },
                ))
                .unwrap()]
            })
        };
        let label = AgentLabel::parse("codex").unwrap();
        let render = |failure| {
            let items = parse_wait_json(wire(failure).to_string().as_bytes()).unwrap();
            render_wait_items(&items, &label, Role::Reviewer)
        };
        let broken = render(true);
        assert!(
            broken.contains("command source BROKEN: deploys"),
            "{broken}"
        );
        assert!(
            !render(false).contains("BROKEN"),
            "an ordinary event is not a breakage"
        );
        // Still ackable: a report the agent cannot finish wakes it
        // forever.
        assert!(
            broken.contains("clank events ack command-deploys-0000abcd@2"),
            "{broken}",
        );
    }

    #[test]
    fn external_wake_hints_render_their_payload() {
        // external-wake-hints-carry-payload: the codex in-hook hints
        // for the external kinds carry the payload instead of the
        // useless `? @ ?` fallback. Built from wait's ACTUAL --json
        // field names (the same wire the hook consumes).
        let raw = serde_json::json!({
            "items": [
                {
                    "kind": "github_event",
                    "repo": "o/r",
                    "event": "pr_comment",
                    "detail": "review",
                    "number": 12,
                    "title": "Add the widget",
                    "actor": "hubot",
                    "url": "https://github.com/o/r/pull/12"
                },
                // The REAL wire, serialized from a real item: the
                // last fixture here was hand-written and still
                // carried `exit_code`/`output_tail` long after the
                // wake stopped having them
                // (a-command-is-an-event-source-like-any-other).
                serde_json::to_value(crate::cli::wait::render_json(
                    &clank_core::wait::WaitItem::CommandEvent {
                        name: "signal".into(),
                        id: "sha-abc".into(),
                        event: "deployed".into(),
                        content: "prod is live".into(),
                        at: None,
                        failure: false,
                        selector: Some("command-signal-0000abcd@7".into()),
                    },
                ))
                .unwrap(),
                { "kind": "some_future_kind", "plan": "p", "sha": "abcdef123456789" }
            ]
        });
        let items = parse_wait_json(raw.to_string().as_bytes()).unwrap();
        let label = AgentLabel::parse("codex").unwrap();
        let out = render_wait_items(&items, &label, Role::Reviewer);
        assert!(
            out.contains(
                "  - github: pr_comment (review) o/r #12 \"Add the widget\" by hubot \
                 https://github.com/o/r/pull/12"
            ),
            "github hint carries the payload: {out}"
        );
        assert!(
            out.contains(
                "  - command: signal deployed: prod is live — ack \
                 `clank events ack command-signal-0000abcd@7`"
            ),
            "the command hint carries the payload AND how to finish it: {out}"
        );
        // Unknown future kinds still fall through loose (fail-soft).
        assert!(
            out.contains("  - some_future_kind: p @ abcdef123456"),
            "unknown kind falls through: {out}"
        );
        // Promptless: no follow-on line anywhere.
        assert!(!out.contains('↳'), "no instructions line: {out}");
        // A prompted watch's instructions render as the sanitized
        // indented follow-on (github-watch-prompts) on THIS surface
        // too — the codex/opencode hook is the real wake wire.
        let raw = serde_json::json!({
            "items": [{
                "kind": "github_event",
                "repo": "o/r",
                "event": "issue_opened",
                "instructions": "Label it,\nthen reply.\tThen ack."
            }]
        });
        let items = parse_wait_json(raw.to_string().as_bytes()).unwrap();
        let out = render_wait_items(&items, &label, Role::Reviewer);
        assert!(
            out.contains("  - github: issue_opened o/r\n    ↳ Label it, then reply. Then ack.\n"),
            "instructions follow-on: {out}"
        );
    }

    /// Deserialize `json!` values through the real `WaitItem` path —
    /// the same typed parse the production stop hook uses — then
    /// render. Exercises both the deserialize and the projection.
    fn items_text(items: &[serde_json::Value]) -> String {
        let parsed: Vec<WaitItem> = items
            .iter()
            .map(|v| serde_json::from_value(v.clone()).unwrap())
            .collect();
        render_wait_items(
            &parsed,
            &AgentLabel::parse("codex").unwrap(),
            Role::Reviewer,
        )
    }

    const FULL_SHA: &str = "f6feba231685eb198eb412e5d014de836c4ddf81";

    #[test]
    fn reviewer_item_is_a_one_line_hint() {
        let out = items_text(&[serde_json::json!({
            "kind": "reviewer",
            "plan": "some-plan",
            "sha": FULL_SHA,
        })]);
        assert!(
            out.contains("  - reviewer: review some-plan @ f6feba231685\n"),
            "got:\n{out}"
        );
    }

    #[test]
    fn wake_carries_no_tutorial_strings() {
        // The HOW lives in the skill doc, not the wake
        // (wait-output-is-a-minimal-hint). One wake with every
        // tutorial-bearing kind must contain none of the old
        // reference material.
        let out = items_text(&[
            serde_json::json!({"kind": "reviewer", "plan": "p", "sha": FULL_SHA}),
            serde_json::json!({"kind": "promote_from_queue", "name": "n", "priority": 300}),
            serde_json::json!({"kind": "blocked", "agent": "claude", "name": "q",
                               "plan": "p", "question": "full question text"}),
            serde_json::json!({"kind": "adhoc_review", "sha": FULL_SHA}),
        ]);
        for tutorial in [
            "feedback write", // the spelled-out command
            "FINISHED",       // the verdict essay
            "continue|finished|request-changes",
            "clank unblock",      // the spelled-out unblock
            "evaluate whether",   // the promote walkthrough
            "full question text", // block question is for the human
            FULL_SHA,             // full sha never appears in the hint
        ] {
            assert!(
                !out.contains(tutorial),
                "wake must not carry `{tutorial}`; got:\n{out}"
            );
        }
    }

    /// A real message item, through the wait's own JSON, renders as
    /// its line and says what ends it — so the two sides' field names
    /// cannot drift apart.
    #[test]
    fn a_message_renders_its_handle_sender_and_subject() {
        let item = |from: Option<&str>, repo: Option<&str>| {
            serde_json::to_value(crate::cli::wait::render_json(
                &clank_core::wait::WaitItem::Message {
                    handle: "500-00000001-rebase-please".to_string(),
                    from: from.map(String::from),
                    repo: repo.map(String::from),
                    subject: "Rebase please".to_string(),
                    body: "# Rebase please\n\nonto master".to_string(),
                    selector: Some("messages@1".to_string()),
                },
            ))
            .unwrap()
        };
        let out = items_text(&[item(Some("claude"), Some("/src/clank")), item(None, None)]);
        let ack = "— ack `clank events ack messages@1`";
        assert!(
            out.contains(&format!(
                "  - message: 500-00000001-rebase-please from claude (/src/clank): \
                 Rebase please {ack}\n"
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "  - message: 500-00000001-rebase-please from a person: Rebase please {ack}\n"
            )),
            "{out}"
        );
        assert!(
            out.contains("fires until the message is acked — `clank events ack`"),
            "{out}"
        );
    }

    #[test]
    fn blocked_and_promote_are_one_liners() {
        let out = items_text(&[
            serde_json::json!({"kind": "blocked", "agent": "claude", "name": "q",
                               "plan": "foo", "question": "?"}),
            serde_json::json!({"kind": "promote_from_queue", "name": "next-up", "priority": 42}),
        ]);
        assert!(out.contains("  - blocked: claude/q on foo (awaiting human)\n"));
        assert!(out.contains("  - promote: next-up (priority 042)\n"));
    }

    #[test]
    fn multiple_plans_open_nudge_carries_the_ordered_remedies() {
        // The one deliberately non-minimal nudge: the remedies are
        // parameterized by the plan names, so they ride in the wake
        // itself rather than the static skill doc
        // (soft-disallow-multiple-plans). The stash ORDER is the
        // load-bearing content: new plan first.
        let out = items_text(&[serde_json::json!({
            "kind": "multiple_plans_open",
            "in_progress": "old-plan",
            "new_plans": ["new-plan"],
            "sha": FULL_SHA,
        })]);
        assert!(out.contains("multiple-plans-open"), "{out}");
        assert!(out.contains("`old-plan`"), "{out}");
        assert!(out.contains("`new-plan`"), "{out}");
        let push_new = out.find("stash push new-plan").expect("pushes new");
        let push_old = out
            .find("stash push old-plan --for new-plan")
            .expect("pushes in-progress with --for");
        assert!(push_new < push_old, "new plan stashed FIRST:\n{out}");
        for n in ["1.", "2.", "3."] {
            assert!(out.contains(n), "all three remedies:\n{out}");
        }
    }

    #[test]
    fn unblocked_answer_is_signal_and_kept() {
        let out = items_text(&[serde_json::json!({
            "kind": "unblocked", "name": "q", "answer": "yes, proceed with B"
        })]);
        assert!(
            out.contains("  - unblocked: q: yes, proceed with B\n"),
            "the human's answer is the action payload; got:\n{out}"
        );
    }

    #[test]
    fn parse_wait_json_reads_typed_envelope() {
        // typed-json-not-json-macro: the stop hook deserializes wait's
        // `--json` envelope into typed `WaitItem`s rather than walking
        // a `serde_json::Value`. `json!` here expresses the wait output
        // the hook consumes across the subprocess boundary.
        let raw = serde_json::json!({
            "items": [
                {"kind": "reviewer", "plan": "p", "sha": FULL_SHA},
                {"kind": "master", "plan": "p", "sha": FULL_SHA,
                 "next": "revise", "reason": "address_commit_changes"},
            ]
        })
        .to_string();
        let items = parse_wait_json(raw.as_bytes()).unwrap();
        assert_eq!(items.len(), 2);
        // TYPED by the parse, not a string a consumer re-reads.
        assert_eq!(
            items[0].wake_kind(),
            Some(clank_core::wait::WaitKind::Reviewer)
        );
        assert_eq!(items[1].next.as_deref(), Some("revise"));
    }

    #[test]
    fn an_empty_normal_wait_is_not_idle() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        let label = AgentLabel::parse("codex").unwrap();

        // Exit 2 used to mean "timed out, go quiet". It has no
        // special meaning now, so it must surface rather than be
        // swallowed as a routine idle.
        let two = outcome_from_wait_output(&out(2, ""), &label, Role::Master, &[]);
        assert!(
            matches!(two, HookOutcome::Diagnostic { .. }),
            "a non-zero wait exit must surface, got {two:?}"
        );

        // A normal wait parks until it has items. Empty success is a
        // broken wait contract, so neither adapter may treat it as idle.
        let empty = outcome_from_wait_output(&out(0, r#"{"items":[]}"#), &label, Role::Master, &[]);
        assert!(
            matches!(empty, HookOutcome::Diagnostic { .. }),
            "empty normal wait must surface, got {empty:?}"
        );
    }

    #[tokio::test]
    async fn the_owner_handle_is_held_for_the_whole_wait() {
        // The regression this guards: anything that closes the
        // child's stdin early — reverting to `Command::output()`, or
        // dropping the handle before the await — gives every in-hook
        // wait instant EOF and exits it immediately.
        //
        // `cat` stands in for the wait (this repo bans spawning the
        // clank binary in tests): it runs until its stdin closes, so
        // if the handle were released early it would exit at once.
        let mut cmd = Command::new("cat");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let owned = OwnedWait::spawn(&mut cmd).expect("cat spawns with a pipe");
        let running = tokio::spawn(async move { owned.run().await });
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let exited = running.is_finished();
        running.abort();
        assert!(
            !exited,
            "the child saw EOF while the wait was still running — the \
             owner handle was released early"
        );
    }

    #[test]
    fn parse_wait_json_is_fail_soft_on_unknown_kind_and_extra_fields() {
        // The subprocess boundary must stay loose: a future wait kind
        // or extra field can NEVER break parsing (the hook never fails
        // the agent). Unknown `kind` renders via the catchall arm;
        // unknown fields are ignored.
        let raw = serde_json::json!({
            "items": [
                {"kind": "future_kind", "plan": "p", "sha": FULL_SHA,
                 "some_new_field": {"nested": 1}},
            ]
        })
        .to_string();
        let items = parse_wait_json(raw.as_bytes()).unwrap();
        let out = render_wait_items(&items, &AgentLabel::parse("codex").unwrap(), Role::Reviewer);
        assert!(
            out.contains("  - future_kind: p @ f6feba231685\n"),
            "unknown kind must render via the catchall arm; got:\n{out}"
        );
    }

    // ── the check-in (attending-checks-in-when-the-work-runs-long) ──

    /// A proven record for THIS process, expected to take `secs`,
    /// whose clock started `ago` seconds back on the wall clock.
    fn expected(ago: i64, secs: u64) -> Attending {
        let pid = std::process::id() as i32;
        let since = (time::OffsetDateTime::now_utc() - time::Duration::seconds(ago))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        Attending {
            expect: Some(Expectation { since, secs }),
            desc: Some("cargo build".into()),
            token: crate::proc_identity::token_for(pid),
            task: "cargo build".into(),
            pid: Some(pid),
            correlation: Correlation::RunDesc,
        }
    }

    fn far_ceiling() -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(86_400)
    }

    fn holder_of(dir: &Path) -> Option<String> {
        std::fs::read_to_string(holder_path(dir)).ok()
    }

    /// Drive `attend` with an injected liveness flag, and something to
    /// do to the agent dir partway through the wait. Returns the
    /// outcome and how much virtual time the wait took.
    async fn attend_with(
        dir: &Path,
        rec: &Attending,
        ceiling: std::time::Instant,
        alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
        meanwhile: impl FnOnce() + Send + 'static,
        after: std::time::Duration,
    ) -> (HookOutcome, std::time::Duration) {
        let start = tokio::time::Instant::now();
        let side = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            meanwhile();
        });
        let proven = Proven {
            rec,
            task_id: "bq1x2y3z4".into(),
        };
        let out = attend(
            &proven,
            dir,
            ceiling,
            || alive.load(std::sync::atomic::Ordering::SeqCst),
            || true,
        )
        .await;
        side.abort();
        (outcome_of(&out), tokio::time::Instant::now() - start)
    }

    fn flag(v: bool) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(v))
    }

    const FIVE_M: std::time::Duration = std::time::Duration::from_secs(300);

    /// The bound is measured from the marker's wall-clock `since`, so
    /// the virtual wait is five minutes less the sliver of real time
    /// between writing the record and arming the timer.
    fn waited_the_bound(took: std::time::Duration) -> bool {
        took + std::time::Duration::from_secs(1) >= FIVE_M
    }

    #[tokio::test(start_paused = true)]
    async fn the_work_ending_before_the_bound_yields_without_a_check_in() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        let alive = flag(true);
        let a = alive.clone();
        let (out, took) = attend_with(
            dir.path(),
            &rec,
            far_ceiling(),
            alive,
            move || a.store(false, std::sync::atomic::Ordering::SeqCst),
            std::time::Duration::from_secs(60),
        )
        .await;
        assert!(is_attending_silence(&out), "{out:?}");
        assert!(
            took < FIVE_M,
            "yielded as soon as the work ended, not at the bound"
        );
        assert_eq!(
            holder_of(dir.path()).as_deref(),
            Some(rec.expect.as_ref().unwrap().since.as_str()),
            "the holder is left alone on the way out"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_bound_arriving_with_the_work_live_checks_in() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        let (out, took) = attend_with(
            dir.path(),
            &rec,
            far_ceiling(),
            flag(true),
            || {},
            FIVE_M * 10,
        )
        .await;
        let HookOutcome::Continue { reason } = &out else {
            panic!("the bound wakes: {out:?}");
        };
        assert!(
            waited_the_bound(took),
            "waited the whole expectation: {took:?}"
        );
        let pid = std::process::id();
        assert!(
            reason.contains(&format!("\"cargo build\" (pid {pid})")),
            "{reason}"
        );
        assert!(reason.contains("you expected 5m"), "{reason}");
        assert!(reason.contains("nothing has been cancelled"), "{reason}");
        assert!(reason.contains("check-in, not a task"), "{reason}");
        assert!(
            reason.contains(&format!(
                "clank attending bq1x2y3z4 --desc 'cargo build' --pid {pid} --expect 10m"
            )),
            "a whole, pasteable recipe: {reason}"
        );
        assert_eq!(
            holder_of(dir.path()).as_deref(),
            Some(rec.expect.as_ref().unwrap().since.as_str()),
            "firing does not delete the holder either"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_bound_already_past_at_entry_checks_in_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(600, 300);
        let (out, took) =
            attend_with(dir.path(), &rec, far_ceiling(), flag(true), || {}, FIVE_M).await;
        let HookOutcome::Continue { reason } = &out else {
            panic!("{out:?}");
        };
        assert!(
            took < std::time::Duration::from_secs(1),
            "no waiting: {took:?}"
        );
        assert!(
            reason.contains("running for 10m — you expected 5m"),
            "elapsed is the WORK's age, from the marker: {reason}"
        );
    }

    /// The final ask is load-bearing: work that ended inside the last
    /// poll interval has a completion wake in flight already.
    #[tokio::test(start_paused = true)]
    async fn work_ending_inside_the_last_poll_interval_is_not_checked_in_on() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        let alive = flag(true);
        let a = alive.clone();
        let (out, _) = attend_with(
            dir.path(),
            &rec,
            far_ceiling(),
            alive,
            move || a.store(false, std::sync::atomic::Ordering::SeqCst),
            FIVE_M - std::time::Duration::from_secs(1),
        )
        .await;
        assert!(is_attending_silence(&out), "{out:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_re_attend_supersedes_the_armed_check_in() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        let holder = holder_path(dir.path());
        let (out, took) = attend_with(
            dir.path(),
            &rec,
            far_ceiling(),
            flag(true),
            move || std::fs::write(&holder, "2099-01-01T00:00:00.000000001Z").unwrap(),
            std::time::Duration::from_secs(60),
        )
        .await;
        // NOT the same silence as work ending: a newer attendance is
        // attending now, and this turn yielded to nobody. One label
        // for both said the opposite (the-turn-end-is-one-decision).
        assert_eq!(
            out,
            HookOutcome::Silent {
                why: SilentReason::AttendanceCleared
            },
            "a newer attendance owns the check-in"
        );
        assert!(took < FIVE_M);
    }

    #[tokio::test(start_paused = true)]
    async fn clear_cancels_the_armed_check_in() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        // A marker for the NEXT turn-end, which --clear must also take.
        write_provable_attending(dir.path(), "next");
        let agent_dir = dir.path().to_path_buf();
        let cleared = std::sync::Arc::new(std::sync::Mutex::new(None));
        let c = cleared.clone();
        let (out, took) = attend_with(
            dir.path(),
            &rec,
            far_ceiling(),
            flag(true),
            move || *c.lock().unwrap() = Some(clear_attendance(&agent_dir).unwrap()),
            std::time::Duration::from_secs(60),
        )
        .await;
        // And a CLEAR is its own answer: the attendance was
        // cancelled, which is neither a live waker nor a takeover.
        assert_eq!(
            out,
            HookOutcome::Silent {
                why: SilentReason::AttendanceCleared
            },
        );
        assert!(took < FIVE_M, "yielded at the clear, not the bound");
        let cleared = cleared.lock().unwrap().take().expect("the clear ran");
        assert!(
            cleared.marker && cleared.check_in,
            "it found and removed both"
        );
        assert!(!attending_path(dir.path()).exists());
        assert!(holder_of(dir.path()).is_none());
    }

    /// A turn-end that attended nothing is not a withdrawal: the sweep
    /// takes the marker and the projection, and the check-in stays.
    #[tokio::test(start_paused = true)]
    async fn an_unrelated_turn_end_leaves_the_check_in_armed() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        record_attended(dir.path(), &rec);
        let agent_dir = dir.path().to_path_buf();
        let (out, took) = attend_with(
            dir.path(),
            &rec,
            far_ceiling(),
            flag(true),
            move || {
                let _ = consume_attending(&agent_dir);
                assert!(
                    read_attended(&agent_dir).is_none(),
                    "the projection was swept"
                );
            },
            std::time::Duration::from_secs(60),
        )
        .await;
        assert!(
            matches!(out, HookOutcome::Continue { .. }),
            "still fires: {out:?}"
        );
        assert!(waited_the_bound(took), "{took:?}");
    }

    /// A DISARM IN THE LAST WINDOW still reports itself.
    ///
    /// The poll loop checks every 2s, so a takeover that lands
    /// between the final poll and the ceiling is seen by NEITHER —
    /// unless the ceiling asks once more. Dropping that ask survived
    /// the whole suite, because every other disarm test lands early
    /// enough for the loop to catch it: the window this covers is the
    /// only place the ceiling's question changes an answer.
    ///
    /// Timings are exact under a paused clock: polls land on even
    /// seconds, the takeover at 28.5s is after the 28s poll, and the
    /// 29s ceiling fires before the loop wakes again at 30s.
    #[tokio::test(start_paused = true)]
    async fn a_disarm_between_the_last_poll_and_the_ceiling_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        let p = holder_path(dir.path());
        let (out, _) = attend_with(
            dir.path(),
            &rec,
            std::time::Instant::now() + std::time::Duration::from_secs(29),
            flag(true),
            move || std::fs::write(&p, "2099-01-01T00:00:00.000000001Z").unwrap(),
            std::time::Duration::from_millis(28_500),
        )
        .await;
        assert_eq!(
            out,
            HookOutcome::Silent {
                why: SilentReason::AttendanceCleared
            },
            "a takeover in the last window is still a takeover, not a live attendance",
        );
    }

    /// The ceiling never checks in — and it ASKS what it hit.
    ///
    /// It used to answer `PollDeadline` whatever the state was,
    /// sharing that label with the in-hook wait's own expiry: one
    /// name over two situations a caller has to tell apart. The work
    /// being live at the ceiling means its completion is still the
    /// waker, and that is one call away rather than a guess.
    #[tokio::test(start_paused = true)]
    async fn the_ceiling_asks_what_it_hit_and_never_checks_in() {
        let rec = expected(0, 300);
        let ceiling = || std::time::Instant::now() + std::time::Duration::from_secs(30);

        // STILL ARMED at the ceiling: the work is live, so its
        // completion wakes the agent. A named waker, not a silence.
        let dir = tempfile::tempdir().unwrap();
        let (out, took) =
            attend_with(dir.path(), &rec, ceiling(), flag(true), || {}, FIVE_M * 10).await;
        assert_eq!(
            out,
            HookOutcome::Silent {
                why: SilentReason::AttendingBackgroundTask
            },
        );
        assert!(took < FIVE_M, "expired at the ceiling: {took:?}");
        // A disarm that lands BEFORE the ceiling reports itself, not
        // the deadline — `a_re_attend_supersedes_the_armed_check_in`
        // and `clear_cancels_the_armed_check_in` are that half.
    }

    /// A record from before the field: the default expectation, from
    /// now. Never an instant check-in for a marker that said nothing.
    #[tokio::test(start_paused = true)]
    async fn a_marker_without_an_expectation_earns_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Attending {
            expect: None,
            ..expected(0, 1)
        };
        let (out, took) = attend_with(
            dir.path(),
            &rec,
            far_ceiling(),
            flag(true),
            || {},
            FIVE_M * 10,
        )
        .await;
        assert!(matches!(out, HookOutcome::Continue { .. }), "{out:?}");
        assert!(waited_the_bound(took), "{took:?}");
        assert!(
            holder_of(dir.path()).is_some(),
            "and an identity was minted for it"
        );
    }

    /// The recipe is one shell-safe line however the description or
    /// id reads: user text must arrive as the same letters, not run.
    #[test]
    fn the_recipe_is_shell_safe_for_any_description_or_id() {
        let rec = Attending {
            desc: Some("say $(rm -rf /) `id` \"hi\" it's".into()),
            ..expected(0, 300)
        };
        let reason = check_in_reason(
            &rec,
            rec.expect.as_ref().unwrap(),
            "b$1",
            time::OffsetDateTime::now_utc(),
        );
        let pid = std::process::id();
        assert!(
            reason.contains(&format!(
                "clank attending 'b$1' --desc 'say $(rm -rf /) `id` \"hi\" it'\\''s' --pid {pid} \
                 --expect 10m"
            )),
            "{reason}"
        );
    }

    /// Both hooks that reach the proof: the asyncRewake hook holds the
    /// timer and — with the bound past — checks in at once, carrying
    /// the harness id of the run it matched; the synchronous hook
    /// cannot hold a timer and silences as it always did.
    /// Returns the dir too: the attended PROJECTION is written by
    /// both attended plans, and a helper that drops the dir cannot
    /// say so.
    async fn proven_run_marker_outcome(
        async_loop: bool,
    ) -> (HookOutcome, tempfile::TempDir, PathBuf) {
        use crate::cli::teams_config::{AgentDescription, RosterRole};
        let dir = init_repo();
        let repo = dir.path();
        let sess = if async_loop {
            "sess-run-async"
        } else {
            "sess-run-sync"
        };
        let label = bind(repo, "claude", sess);
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::On).unwrap();
        crate::cli::agent::add_repo_roster_agent(
            repo,
            &label,
            AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
            RosterRole::Commit,
        )
        .unwrap();
        crate::cli::agent::set_repo_master(repo, &label).unwrap();
        let agent_dir = crate::agent_store::agents_root(repo).join("claude");
        std::fs::create_dir_all(&agent_dir).unwrap();
        // What `clank run` wrote — the description as the task, no
        // harness id — ten minutes ago, expecting five.
        let rec = expected(600, 300);
        std::fs::write(
            agent_dir.join("attending"),
            serde_json::to_string(&rec).unwrap(),
        )
        .unwrap();
        let input: HookInput = serde_json::from_value(serde_json::json!({
            "session_id": sess,
            "cwd": "/tmp",
            "stop_hook_active": false,
            "background_tasks": [
                {"id": "bq9run", "command": "clank run --desc \"cargo build\" -- cargo build"}
            ],
        }))
        .unwrap();
        let out = compute_outcome_with(
            Tool::Claude,
            Some(repo),
            input,
            async_loop,
            std::time::Instant::now(),
            &stated(),
        )
        .await;
        (out, dir, agent_dir)
    }

    #[tokio::test]
    async fn the_check_in_for_a_run_marker_carries_the_matched_harness_id() {
        let (out, _dir, agent_dir) = proven_run_marker_outcome(true).await;
        let HookOutcome::Continue { reason } = &out else {
            panic!("bound past, work live: {out:?}");
        };
        assert!(
            reason.contains(&format!(
                "clank attending bq9run --desc 'cargo build' --pid {} --expect 10m",
                std::process::id()
            )),
            "the id the marker could not know, learned at the proof: {reason}"
        );
        // The projection too: the attended plan that HOLDS the timer
        // records the same thing the one that yields does. Asserting
        // it on only one of them left the other's write unproven.
        assert!(
            read_attended(&agent_dir).is_some(),
            "an attended turn records what it attends",
        );
    }

    /// The entry decision for a run the tool listed without an id:
    /// the marker is consumed, nothing is proven, so nothing is
    /// recorded and no timer is armed — the turn goes on to park, as
    /// any unproven attendance does.
    #[test]
    fn a_run_match_without_an_id_is_not_proven_and_arms_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let rec = expected(0, 300);
        std::fs::write(
            attending_path(dir.path()),
            serde_json::to_string(&rec).unwrap(),
        )
        .unwrap();
        let attending = consume_attending(dir.path());
        let turn = runs_live(&[(None, "cargo build")]);
        assert!(
            prove_and_record(attending.as_ref(), &turn, dir.path()).is_none(),
            "a match that cannot be re-attended proves nothing"
        );
        assert!(
            !attending_path(dir.path()).exists(),
            "consumed all the same"
        );
        assert!(read_attended(dir.path()).is_none(), "no decision recorded");
        assert!(holder_of(dir.path()).is_none(), "no check-in armed");
        // The same turn WITH the id is the proof, and yields it.
        let turn = runs_live(&[(Some("bq9run"), "cargo build")]);
        let proven = prove_and_record(Some(&rec), &turn, dir.path()).expect("proven");
        assert_eq!(proven.task_id, "bq9run");
    }

    #[tokio::test]
    async fn outside_the_async_loop_attendance_silences_at_once() {
        let (out, _dir, agent_dir) = proven_run_marker_outcome(false).await;
        assert!(is_attending_silence(&out), "{out:?}");
        // And it still RECORDS what it is attending. The timer is
        // what the legacy hook cannot hold; the projection the TUI
        // reads is not, and routing this arm through a plain decision
        // silently dropped it.
        assert!(
            read_attended(&agent_dir).is_some(),
            "a legacy attended yield records what it attends",
        );
    }

    #[test]
    fn a_marker_without_an_expectation_still_parses() {
        let rec: Attending = serde_json::from_str(
            r#"{"task":"t","desc":"test run","pid":1,"correlation":"task_id"}"#,
        )
        .unwrap();
        assert!(rec.expect.is_none());
        let rec: Attended =
            serde_json::from_str(r#"{"task":"t","pid":1,"at":"2026-08-20T14:51:09Z"}"#).unwrap();
        assert!(rec.expect.is_none());
    }

    #[test]
    fn the_projection_ages_the_work_against_its_bound() {
        let now = time::OffsetDateTime::now_utc();
        let at = |secs_ago: i64| {
            (now - time::Duration::seconds(secs_ago))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let mut rec = Attended {
            task: "t".into(),
            expect: Some(Expectation {
                since: at(180),
                secs: 300,
            }),
            desc: None,
            pid: None,
            token: None,
            at: at(10),
        };
        assert_eq!(rec.age(now).as_deref(), Some("3m/5m"));
        rec.expect.as_mut().unwrap().since = at(720);
        assert_eq!(
            rec.age(now).as_deref(),
            Some("12m/5m"),
            "overdue says so by itself"
        );
        rec.expect = None;
        assert_eq!(
            rec.age(now).as_deref(),
            Some("10s"),
            "without one: from the decision, as before"
        );
    }
}
