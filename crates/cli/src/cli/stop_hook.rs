//! `clank stop-hook --tool <claude|codex>` — the Stop-hook
//! adapter installed into `~/.claude/settings.json` and
//! `~/.codex/hooks.json` by `clank setup`.
//!
//! Reads the hook stdin JSON via `clank_core::HookInput`,
//! computes a [`HookOutcome`] based on the calling agent's
//! `AgentConfig.auto_mode`, and renders the outcome to the
//! per-tool wire shape (claude: exit 2 + stderr; codex: exit
//! 0 + stdout JSON; everything else exit 0).
//!
//! Both tools now long-poll IN-HOOK, by different routes. **claude**
//! parks inside an `asyncRewake` Stop hook, which runs in the
//! background and wakes the session on exit 2 with the hook's stderr
//! as a system reminder (claude-asyncrewake-work-loop). That replaced
//! arming a background `clank wait`, which Claude Code's task reaper
//! kills; the arm model SURVIVES for claude versions without the
//! asyncRewake capability (`LoopPolicy::BackgroundArm`), which nudge
//! and never park. **codex** has no async hook
//! support at all — the field is parsed but unimplemented, and setting
//! it makes codex SKIP the hook — so its hook blocks in-hook and
//! returns the rendered items.
//!
//! The poll has THREE terminators: a wake, its own deadline, or the
//! hook's OWN death. The deadline is derived from the ceiling clank
//! writes into the hook config and expires cleanly BELOW it, because
//! being killed at the ceiling surfaces to the user as a failed hook
//! (codex-idle-is-not-a-hook-failure). Death it learns by holding the
//! write end of the wait's stdin pipe (remove-wait-timeout): if this
//! process is killed — SIGKILL included, where no destructor runs —
//! the pipe closes and the wait reaps itself.
//!
//! **The hook NEVER fails the agent.** Every error path
//! produces [`HookOutcome::Diagnostic`] which exits 0 with a
//! human-readable stderr line. A non-zero exit from this binary
//! is a clank bug.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::{StopHookArgs, resolve_repo};
use crate::agent_env::resolve_identity_for_hook;
use crate::agent_store::load_agent_config;
use crate::lifecycle::AgentLabel;
use clank_core::{
    AutoMode, BgDisposition, CLAUDE_CONTINUATION_EXIT, CodexBlockDecision, HOOK_OK_EXIT, HookInput,
    HookOutcome, Role, SilentReason, Tool, background_disposition,
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
    let async_loop =
        tool == Tool::Claude && args.loop_mode == Some(crate::cli::LoopModeArg::Asyncrewake);
    let outcome = match read_hook_stdin() {
        Ok(input) => {
            compute_outcome_with(tool, args.repo.as_deref(), input, async_loop, started).await
        }
        Err(e) => HookOutcome::Diagnostic { message: e },
    };
    emit_and_exit(outcome, tool);
}

/// Test-facing wrapper: production routes through
/// [`compute_outcome_with`] carrying the argv-selected loop mode.
#[cfg(test)]
async fn compute_outcome(
    tool: Tool,
    repo_override: Option<&Path>,
    input: HookInput,
) -> HookOutcome {
    compute_outcome_with(tool, repo_override, input, false, std::time::Instant::now()).await
}

async fn compute_outcome_with(
    tool: Tool,
    repo_override: Option<&Path>,
    input: HookInput,
    async_loop: bool,
    started: std::time::Instant,
) -> HookOutcome {
    // Decide how the agent's in-flight background work affects this
    // turn-end, BEFORE any clank resolution (`background_disposition` is a
    // pure function of the turn). `YieldArmed` never runs a wait — Claude
    // Code auto-wakes when the work completes, and running our own wait here
    // would block that wake. Only `NeedsWorkCheck` / `NoBackgroundWork` fall
    // through to resolution, so a config Diagnostic can only surface when we
    // were going to engage clank anyway.
    let live_ids = input.live_task_ids();
    let disposition = background_disposition(tool, &input);
    if matches!(disposition, BgDisposition::YieldArmed) {
        return HookOutcome::Silent {
            why: SilentReason::YieldArmed,
        };
    }

    // Resolve repo: explicit --repo (testing) > hook stdin cwd.
    let repo = match resolve_hook_repo(repo_override, &input) {
        Ok(r) => r,
        Err(e) => return HookOutcome::Diagnostic { message: e },
    };

    let label = match resolve_identity_for_hook(&repo, tool, &input.session_id) {
        Ok(l) => l,
        Err(e) => {
            return HookOutcome::Diagnostic {
                message: format!("{e:#}"),
            };
        }
    };

    // Take the marker NOW, before any branch can return without it.
    // `AutoMode::Off` and the empty-items path both used to exit
    // above the old reap, which is how markers outlived their turn.
    let agent_dir = crate::agent_store::agents_root(&repo).join(label.as_str());
    let attending = consume_attending(&agent_dir);

    // Config may be ABSENT (bound via `clank as` but auto never
    // touched) — that's not "off", it's "unset, inherit the
    // ~/.clank default" (auto-mode-default-on). Resolve the
    // effective mode through the shared resolver so a fresh session
    // under a user-global default-on actually drives, instead of
    // silently exiting (codex b29d8d0).
    let cfg = match load_agent_config(&repo, &label) {
        Ok(c) => c,
        Err(e) => {
            return HookOutcome::Diagnostic {
                message: format!("{e:#}"),
            };
        }
    };
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let effective = crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref());

    match effective {
        AutoMode::Off => HookOutcome::Silent {
            why: SilentReason::AutoOff,
        },
        // Role is resolved HERE, inside the arm that drives — never
        // above the auto-mode check. An agent with auto off has asked
        // not to be driven, and diagnosing its repo's roster would be
        // noise about work it is not going to do.
        //
        // A FAILURE is not laundered into an answer. Defaulting to
        // Reviewer was the opposite of the conservative choice it
        // claimed to be: for an agent that is master it arms a poll
        // that can never return, and where the tool allows one
        // in-flight wait per session nothing can replace it — the
        // session is dead until a human prompts it.
        //
        // So arm nothing, and SAY so. That failure cost hours of dead
        // session undiagnosable from outside; a quiet exit would
        // rebuild exactly that hole.
        AutoMode::On => match crate::agent_store::resolve_role(&repo, &label) {
            Err(e) => HookOutcome::Diagnostic {
                message: format!(
                    "hook: cannot resolve `{}`'s role ({e:#}); arming no wait. \
                     Fix the roster and the next turn-end will arm one.",
                    label.as_str()
                ),
            },
            Ok(role) => {
                // Attendance is decided HERE, before any park, because
                // this is where `live_ids` is FRESH. Deciding it at
                // wait-return meant judging a snapshot taken at hook
                // entry and then carried through an unbounded park —
                // by then the attended task may have finished hours
                // ago and spent its wake, and suppressing on that is
                // the permanent sleep (codex on 81981f2).
                //
                // Every path out of here holds a channel: this branch
                // has just proven a completion wake is outstanding,
                // and every other one parks.
                let run_matches = attending
                    .as_ref()
                    .filter(|r| r.correlation == Correlation::RunDesc)
                    .map_or(0, |r| input.live_clank_run_matches(&r.task));
                if let Some(silence) =
                    attendance_silence(attending.as_ref(), &live_ids, run_matches, &agent_dir)
                {
                    return silence;
                }
                match disposition {
                    // A process is live with no `clank wait` watching. Peek (without
                    // blocking) whether the agent has work RIGHT NOW:
                    //  - it does → it's still its turn, blocked on its own task →
                    //    yield silently (don't nudge; that was the loop — a master
                    //    mid-plan always has "continue" work, so a nudged wait would
                    //    return instantly and never persist).
                    //  - it doesn't → idle (e.g. just committed, awaiting reviews) →
                    //    nudge it to start `clank wait` alongside the process, so
                    //    EITHER the process finishing OR review work wakes it. Safe
                    //    from looping: with no work the wait blocks and persists, and
                    //    the next Stop sees it (`YieldArmed`).
                    // In the asyncrewake loop the PARK is the watcher: a live
                    // background process coexists with the parked wait (its
                    // completion wakes via the task notification; work wakes
                    // via exit 2) — nudging the agent to arm a background
                    // wait would reintroduce the tracked-task loop this mode
                    // exists to delete (claude-asyncrewake-work-loop).
                    BgDisposition::NeedsWorkCheck if async_loop => {
                        return asyncrewake_park(
                            &repo,
                            &label,
                            role,
                            started,
                            &live_ids,
                            deadline_action(tool),
                        )
                        .await;
                    }
                    BgDisposition::NeedsWorkCheck => match peek_has_work(&repo, &label).await {
                        Ok(true) => HookOutcome::Silent {
                            why: SilentReason::BusyOwnWork,
                        },
                        Ok(false) => HookOutcome::Continue {
                            reason: nudge_reason(&input),
                        },
                        // Fail-soft: if the peek can't run, yield rather than nudge
                        // (the process still wakes the agent; no forced turn on doubt).
                        Err(_) => HookOutcome::Silent {
                            why: SilentReason::PeekFailed,
                        },
                    },
                    // NoBackgroundWork (YieldArmed already returned above): genuinely idle.
                    _ if async_loop => {
                        return asyncrewake_park(
                            &repo,
                            &label,
                            role,
                            started,
                            &live_ids,
                            deadline_action(tool),
                        )
                        .await;
                    }
                    _ => match loop_policy(tool) {
                        // LEGACY claude only (`--loop asyncrewake` returns
                        // above): this branch never waits in-hook and never
                        // renders work items — its only continuation is the
                        // arm-the-wait hint, and the armed wait's completion
                        // wake delivers the work. Work presence is deliberately
                        // NOT consulted: if work exists the armed wait exits
                        // immediately and the wake carries it
                        // (claude-stop-hook-minimal-hint).
                        LoopPolicy::BackgroundArm => HookOutcome::Continue {
                            reason: nudge_reason(&input),
                        },
                        // The in-hook long-poll + emit-with-items model: codex
                        // blocks with the items; opencode's plugin injects
                        // non-empty output on session.idle (M1 spike). No
                        // work / timeout is SILENT either way (codex 8000d6e:
                        // a nudge relay would loop an opencode session
                        // forever).
                        LoopPolicy::InHookWait => {
                            compute_wait_outcome(
                                &repo,
                                &label,
                                role,
                                started,
                                &live_ids,
                                deadline_action(tool),
                            )
                            .await
                        }
                        // Grok's hooks are PASSIVE (grok-first-class): clank
                        // installs no grok adapter and no continuation could
                        // drive it. If something wires this up anyway, say so.
                        LoopPolicy::Passive => HookOutcome::Diagnostic {
                            message: "hook: grok has no stop-hook adapter (grok hooks are passive)"
                                .into(),
                        },
                    },
                }
            }
        },
    }
}

/// Which continuation model a tool's stop-hook uses — the PURE
/// tool→loop-policy decision (codex fbc5da1), so routing is testable
/// without the spawning wait path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopPolicy {
    /// Background-armed wait; the hook only nudges (claude).
    BackgroundArm,
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
    /// End the turn with nothing armed.
    Sleep,
}

/// EXHAUSTIVE by design: a new adapter must answer this question
/// rather than inherit whichever arm happened to be the default.
fn deadline_action(tool: Tool) -> DeadlineAction {
    match tool {
        Tool::Claude | Tool::Codex => DeadlineAction::ReArm,
        // opencode's plugin runs its work loop on `session.idle`, so a
        // continuation re-idles immediately and spins forever
        // (codex 8000d6e). It needs a different answer, not this one.
        Tool::OpenCode => DeadlineAction::Sleep,
        // Passive hooks reach no deadline at all; the arm exists so
        // the match stays total.
        Tool::Grok => DeadlineAction::Sleep,
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
        DeadlineAction::Sleep => HookOutcome::Silent {
            why: SilentReason::PollDeadline,
        },
    }
}

fn loop_policy(tool: Tool) -> LoopPolicy {
    match tool {
        Tool::Claude => LoopPolicy::BackgroundArm,
        Tool::Codex | Tool::OpenCode => LoopPolicy::InHookWait,
        Tool::Grok => LoopPolicy::Passive,
    }
}

/// SessionStart (claude-asyncrewake-work-loop): every incarnation
/// MINTS a new wait generation — revoking any waiter surviving from
/// a previous incarnation (whose wake pipe is dead, M0) — then
/// surfaces pending work via `additionalContext` so a resumed
/// session catches up immediately instead of waiting for its first
/// turn-end park. Fail-open EVERYWHERE: a session start must never
/// be broken by clank state, so every error path exits 0 quietly.
async fn run_session_start(args: StopHookArgs) -> anyhow::Result<()> {
    #[derive(serde::Deserialize)]
    struct SessionStartInput {
        session_id: String,
        cwd: String,
    }
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
    let Ok(sid) = clank_core::ids::SessionId::parse(&input.session_id) else {
        return Ok(());
    };
    let Ok(label) = resolve_identity_for_hook(&repo, tool, &sid) else {
        return Ok(());
    };

    // Mint FIRST — stale-waiter revocation must not depend on the
    // peek working (or on auto being on).
    let agent_dir = crate::agent_store::agents_root(&repo).join(label.as_str());
    // Fail-open by POLICY: a session start is never broken by clank
    // state, so the mint error is explicitly discarded here (the
    // bind-time mint is the one that must not fail silently).
    let _ = crate::agent_store::mint_wait_generation(&agent_dir);

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

// ── asyncrewake ownership + park (claude-asyncrewake-work-loop) ──
// One waiter per session, owned GENERATIONALLY: the wake transport
// is the hook's stderr pipe to its own claude process (M0), so a
// waiter surviving its session can never wake the resumed
// incarnation — a newer generation takes the park over and the
// stale waiter suppresses itself.

/// What a new asyncrewake hook does given the park-lease state.
#[derive(Debug, PartialEq, Eq)]
enum ParkDecision {
    /// Lease free — park.
    Park,
    /// A live waiter of THIS generation holds the park — exit quiet.
    DeferToLiveWaiter,
    /// The holder's generation is stale — it self-releases on its
    /// next generation check; retry the lease briefly.
    AwaitStaleHandoff,
}

fn park_decision(lease_free: bool, holder_gen: Option<u64>, current_gen: u64) -> ParkDecision {
    if lease_free {
        ParkDecision::Park
    } else if holder_gen == Some(current_gen) {
        ParkDecision::DeferToLiveWaiter
    } else {
        ParkDecision::AwaitStaleHandoff
    }
}

/// See [`crate::agent_store::read_wait_generation`] — minted by
/// session BINDING and SessionStart alike.
fn read_generation(agent_dir: &Path) -> u64 {
    crate::agent_store::read_wait_generation(agent_dir)
}

fn read_holder_gen(agent_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(agent_dir.join("wait.holder"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// flock RAII over `wait.lock` — exclusive, non-blocking. Same
/// mechanism as the event-WAL leases.
struct WaitLease {
    _file: std::fs::File,
}

fn try_wait_lease(agent_dir: &Path) -> std::io::Result<Option<WaitLease>> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(agent_dir.join("wait.lock"))?;
    // SAFETY: valid owned fd; flock has no memory effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(WaitLease { _file: file }))
    } else {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(e)
        }
    }
}

/// Resolves when the generation file no longer reads `own_gen`.
async fn generation_changed(agent_dir: &Path, own_gen: u64) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if read_generation(agent_dir) != own_gen {
            return;
        }
    }
}

/// Park the SHARED long-poll under this session's generational
/// lease. Work → Continue (claude's exit-2 + stderr wire IS the
/// asyncRewake wake); no work → Silent (empty-is-quiescent is
/// load-bearing — M0's unconditional-exit-2 probe looped forever).
async fn asyncrewake_park(
    repo: &Path,
    label: &AgentLabel,
    role: Role,
    started: std::time::Instant,
    live_ids: &[String],
    on_deadline: DeadlineAction,
) -> HookOutcome {
    let agent_dir = crate::agent_store::agents_root(repo).join(label.as_str());
    if let Err(e) = std::fs::create_dir_all(&agent_dir) {
        return HookOutcome::Diagnostic {
            message: format!("hook: creating `{}`: {e}", agent_dir.display()),
        };
    }
    let my_gen = read_generation(&agent_dir);

    // Take the park, deferring to a live waiter and briefly awaiting
    // a stale one's self-release (it checks its generation every 2s).
    let mut tries = 0u32;
    let lease = loop {
        match try_wait_lease(&agent_dir) {
            Ok(Some(lease)) => break lease,
            Ok(None) => match park_decision(false, read_holder_gen(&agent_dir), my_gen) {
                ParkDecision::DeferToLiveWaiter => {
                    return HookOutcome::Silent {
                        why: SilentReason::WaiterAlreadyParked,
                    };
                }
                ParkDecision::AwaitStaleHandoff => {
                    tries += 1;
                    if tries > 60 {
                        return HookOutcome::Diagnostic {
                            message: "hook: stale waiter did not hand off the park".to_string(),
                        };
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                ParkDecision::Park => unreachable!("lease reported held"),
            },
            Err(e) => {
                return HookOutcome::Diagnostic {
                    message: format!("hook: wait lease: {e}"),
                };
            }
        }
    };
    if let Err(e) = std::fs::write(agent_dir.join("wait.holder"), format!("{my_gen}\n")) {
        return HookOutcome::Diagnostic {
            message: format!("hook: recording park holder: {e}"),
        };
    }

    let outcome = tokio::select! {
        o = compute_wait_outcome(repo, label, role, started, live_ids, on_deadline) => o,
        // A newer incarnation took over: release (the select drops
        // the wait child via kill_on_drop) and suppress.
        () = generation_changed(&agent_dir, my_gen) => {
            return HookOutcome::Silent {
                why: SilentReason::StaleGeneration,
            };
        }
    };
    // Wake guard: never emit for a dead incarnation.
    if read_generation(&agent_dir) != my_gen {
        return HookOutcome::Silent {
            why: SilentReason::StaleGeneration,
        };
    }
    drop(lease);
    outcome
}

/// Non-blocking peek: does `label` have actionable clank work right now?
/// Self-spawns `clank wait --peek --json` (side-effect-free: fires no
/// hooks) and CAPTURES its stdout — never inherits it — so the peek's JSON
/// envelope can't corrupt the hook's own protocol stdout (ruthless c042912).
async fn peek_has_work(repo: &Path, label: &AgentLabel) -> Result<bool, String> {
    peek_items(repo, label).await.map(|i| !i.is_empty())
}

/// The peek's ITEMS — shared by the work-check and SessionStart's
/// catch-up context (claude-asyncrewake-work-loop).
pub(crate) async fn peek_items(repo: &Path, label: &AgentLabel) -> Result<Vec<WaitItem>, String> {
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
        other => Err(format!(
            "peek exited {} stderr={}",
            other
                .map(|c| c.to_string())
                .unwrap_or_else(|| "(signaled)".into()),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The continuation that nudges the agent to arm its own backgrounded
/// `clank wait` — the ONLY continuation the claude hook ever emits, and
/// self-extinguishing: once armed, the next Stop is `YieldArmed`-silent.
/// TERSE by the minimal-hint rule (terse-arm-wait-nudge): the skill docs
/// teach the arm/act/re-arm loop, so the nudge states only the trigger
/// plus the one detail models get wrong (run_in_background). Deliberately
/// does NOT echo the task command lines: the agent knows what it
/// backgrounded, and real commands (multi-clause `until …; do sleep …`
/// one-liners) turned the nudge into a wall of shell that buried the
/// instruction (lloyd, dark-skippy). Only the COUNT is stated. The two
/// variants share the instruction core so they can't drift.
fn nudge_reason(input: &HookInput) -> String {
    const CORE: &str = "run `clank wait` (run_in_background: true), then end your turn.";
    let count = input
        .background_tasks
        .iter()
        .filter(|t| !t.is_clank_wait())
        .count();
    if count == 0 {
        return format!("Nothing is watching for clank work — {CORE}");
    }
    let what = if count > 1 {
        format!("{count} background tasks")
    } else {
        "a background task".to_string()
    };
    format!(
        "You ended your turn with {what} still running but nothing watching \
         for clank work — also {CORE}"
    )
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
fn wait_argv(repo: &Path, label: &AgentLabel) -> Vec<String> {
    vec![
        "wait".into(),
        "--repo".into(),
        repo.display().to_string(),
        "--author".into(),
        label.as_str().to_string(),
        // Ownership, not decoration: without this the wait has no
        // bound at all and outlives a killed hook forever.
        "--die-with-owner".into(),
        "--json".into(),
    ]
}

fn wait_command(exe: &Path, repo: &Path, label: &AgentLabel) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(wait_argv(repo, label))
        // Piped stdin IS the ownership channel. `OwnedWait::spawn`
        // refuses to proceed without it.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
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
) -> HookOutcome {
    // Fixed at hook entry, so everything below — current_exe, command
    // construction, the child spawn — is charged against it.
    let deadline = poll_deadline(started);
    let arm = || {
        let exe = std::env::current_exe().map_err(|e| format!("hook: current_exe failed: {e}"))?;
        let mut cmd = wait_command(&exe, repo, label);
        Ok(OwnedWait::spawn(&mut cmd)?.run())
    };
    match await_wait_under_deadline(deadline, on_deadline, arm).await {
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

/// Where the hook records the attendance decision it last made.
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

fn attended_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("attended")
}

/// The agent's intent for ONE yield: "I am ending my turn because I
/// am waiting on this, do not nudge me for this yield."
///
/// Consumed by the Stop hook that reads it, so it silences at most
/// one turn-end and can never outlive the turn that wrote it. That
/// is the whole safety argument, and it needs no liveness check:
/// staleness requires surviving, and this cannot.
///
/// The earlier design stored the same words as a claim about a
/// DURATION — "X is running, stay silent until it stops" — which had
/// to remain true over time, so it needed reaping, and the only
/// reaper was the Stop hook the marker silenced. A reviewer sat
/// mute for three hours on that.
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attending {
    pub(crate) task: String,
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
    /// answers `false` and keeps its park. Noise, never silence.
    /// `run_matches` is how many live background tasks are a
    /// `clank run` carrying this marker's description. A run owned by
    /// clank has no harness id to match on — the tool assigns one only
    /// after launching the command — so the description is its handle,
    /// and EXACTLY ONE match is required: zero proves nothing, and two
    /// or more cannot say which is this marker's.
    pub(crate) fn provably_live(&self, live_ids: &[String], run_matches: usize) -> bool {
        let (Some(pid), Some(token)) = (self.pid, self.token.as_ref()) else {
            return false;
        };
        // Only the evidence this marker was written for. Accepting
        // either would let each path be satisfied by the other's
        // coincidence.
        let correlated = match self.correlation {
            Correlation::TaskId => live_ids.iter().any(|id| id == &self.task),
            Correlation::RunDesc => run_matches == 1,
        };
        correlated && pid_is_alive(pid) && token.still_holds(pid)
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

    pub(crate) fn age(&self, now: time::OffsetDateTime) -> Option<String> {
        let at =
            time::OffsetDateTime::parse(&self.at, &time::format_description::well_known::Rfc3339)
                .ok()?;
        Some(short_duration(now - at))
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

/// A duration at one significant unit — long enough to judge staleness
/// by eye, short enough for a status line that must not wrap.
fn short_duration(d: time::Duration) -> String {
    let secs = d.whole_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// The last decision, for surfaces that report it. Read-only: status
/// never writes or clears this, because the hook rewrites it on every
/// turn-end anyway.
pub(crate) fn read_attended(agent_dir: &Path) -> Option<Attended> {
    let raw = std::fs::read_to_string(attended_path(agent_dir)).ok()?;
    serde_json::from_str::<Attended>(&raw).ok()
}

/// Take the marker, and take it WHATEVER happens next.
///
/// Unconditional and early by design. Every way the old code failed to
/// reap was "the hook ran but never reached the marker" — auto off,
/// no work, or the task still reported live — so the only deletion
/// that closes them is one no branch can skip. Read-then-delete, never
/// delete-if-used: a marker that survives a Stop for any reason is the
/// duration-scoped bug returning.
///
/// The stale decision record goes with it, so what remains describes
/// THIS turn-end and no earlier one.
fn consume_attending(agent_dir: &Path) -> Option<Attending> {
    let path = attending_path(agent_dir);
    let raw = std::fs::read_to_string(&path).ok();
    if raw.is_some() {
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::remove_file(attended_path(agent_dir));
    // An unreadable marker suppresses nothing — fail toward NOISE,
    // never toward silence.
    raw.and_then(|r| serde_json::from_str::<Attending>(&r).ok())
}

/// The attendance decision, taken at hook ENTRY where `live_ids` is
/// still fresh. `Some` means this turn is silenced and the wake has
/// been handed to the attended task's completion; `None` means carry
/// on and park.
///
/// It lives here, and not at wait-return, because the list handed to
/// a returning park was snapshotted before the park began and may be
/// arbitrarily old by then (codex on 81981f2).
fn attendance_silence(
    attending: Option<&Attending>,
    live_ids: &[String],
    run_matches: usize,
    agent_dir: &Path,
) -> Option<HookOutcome> {
    let rec = attending?;
    if !rec.provably_live(live_ids, run_matches) {
        return None;
    }
    record_attended(agent_dir, rec);
    Some(HookOutcome::Silent {
        why: SilentReason::AttendingBackgroundTask,
    })
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
/// Every non-zero exit is a Diagnostic. There is no longer a code
/// that means "expired quietly": an idle poll parks instead of
/// self-expiring (remove-wait-timeout), so a wait that exits
/// non-zero has genuinely failed and must be surfaced rather than
/// swallowed as a routine idle.
fn outcome_from_wait_output(
    output: &std::process::Output,
    label: &AgentLabel,
    role: Role,
    live_ids: &[String],
) -> HookOutcome {
    match output.status.code() {
        Some(0) => match parse_wait_json(&output.stdout) {
            Ok(items) if items.is_empty() => HookOutcome::Silent {
                why: SilentReason::NoWork,
            },
            Ok(items) => {
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
                         turn with no provable wake is how an agent goes to sleep for good. Do \
                         not end your turn to poll it.",
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
}

/// One `wait --json` item as the stop hook reads it. `kind` is the
/// `#[serde(tag = "kind")]` discriminant `wait` emits; the remaining
/// fields are the ones [`render_wait_items`] projects into the
/// minimal wake hint (`wait-output-is-a-minimal-hint`). Anything
/// `wait` adds is ignored; anything absent stays `None`.
#[derive(serde::Deserialize)]
pub(crate) struct WaitItem {
    pub(crate) kind: Option<String>,
    pub(crate) plan: Option<String>,
    pub(crate) sha: Option<String>,
    finalized_at: Option<String>,
    next: Option<String>,
    pub(crate) reason: Option<String>,
    prompt: Option<String>,
    name: Option<String>,
    priority: Option<u64>,
    agent: Option<String>,
    answer: Option<String>,
    /// Ad-hoc settle verdict (adhoc-continue-wake-investigation).
    gate: Option<String>,
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
    exit_code: Option<i64>,
    output_tail: Option<String>,
}

fn parse_wait_json(raw: &[u8]) -> Result<Vec<WaitItem>, String> {
    let envelope: WaitEnvelope =
        serde_json::from_slice(raw).map_err(|e| format!("not valid JSON: {e}"))?;
    Ok(envelope.items)
}

/// Render wait's JSON `items` array into the continuation prompt
/// body — codex-only; claude's hook never delivers items (see the
/// module doc). Loose stringly-typed projection because we're consuming
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
    /// Fires on an edge and will not repeat. Carries WHY, so a
    /// one-shot is a decision rather than an omission.
    OneShot(&'static str),
    /// Watcher delivery, not stop-hook wake delivery.
    ObserverOnly,
}

/// Classify a rendered item. `reason` matters for `master`, whose
/// several `WaitingReason`s discharge differently.
pub(crate) fn discharge_of(kind: &str, reason: Option<&str>) -> Option<Discharge> {
    use Discharge::*;
    Some(match kind {
        "master" => match reason? {
            // Says only what ENDS it. The `clank attending` recipe is
            // the hint's job (attending-suppresses-standing-wakes),
            // which fires only when a background task is actually
            // live — advertising it unconditionally here would tell an
            // agent with nothing running to record nothing.
            "gate_continue" => Until("fires until a commit moves the gate"),
            "address_commit_changes" => {
                Until("fires until you commit work addressing the feedback on that sha")
            }
            "ready_to_finalize" => Until("fires until `clank finish` runs"),
            "commit_plan_revision" => Until("fires until the plan edit is committed"),
            // A future reason must not silently claim a condition.
            _ => return None,
        },
        "reviewer" => Until("fires until you write feedback for that sha"),
        "fix_commit_tag" => Until("fires until the commit's plan tag is corrected"),
        "multiple_plans_open" => Until(
            "fires until one plan is active — `clank finish` one, or `clank stash push` the rest",
        ),
        "promote_from_queue" => Until("fires until the item is promoted or removed from the queue"),
        "github_event" => Until("fires until the event is acked — `clank events ack`"),
        "adhoc_review" => Until("fires until feedback exists for that sha"),
        "adhoc_revise" => Until("fires until you commit the revision"),
        "pr_reviewer" => Until("fires until your review for that round is written"),
        "pr_master" => Until("fires until you act on that round"),
        "unblocked" => Until("fires until you acknowledge it — `clank block clean`"),
        "blocked" => Until(
            "you CANNOT clear this — only the human can answer it. Do not act on it, and do \
             not answer it yourself",
        ),
        "idle" => Until("fires while there is nothing else to do"),
        "finished" => OneShot("a finalize notice — it will not fire again for that sha"),
        "adhoc_settled" => OneShot("a settle notice — it will not fire again for that sha"),
        // REPEATING, despite looking like a one-off. Command sources
        // are spawned once per wait, and every stop-hook re-arm starts
        // them again — so a source that exits immediately re-fires on
        // every re-arm (extra-wait-events). Classifying it from the
        // single wait process rather than the re-arm loop is how it
        // read as one-shot.
        "command_event" => Until(
            "fires again on every re-arm unless the source BLOCKS until a real event — \
             change or remove the command source to stop it",
        ),
        "for_commit" | "for_finished" | "for_blocked" => ObserverOnly,
        // Unknown/future kinds render WITHOUT a condition rather than
        // guessing: the subprocess boundary stays fail-soft.
        _ => return None,
    })
}

fn discharge_suffix(kind: &str, reason: Option<&str>) -> String {
    match discharge_of(kind, reason) {
        // NOT `↳` — that already marks a watch's standing
        // instructions (`github-watch-prompts`), and two meanings on
        // one glyph is how a reader stops trusting either.
        Some(Discharge::Until(t)) | Some(Discharge::OneShot(t)) => format!("      · {t}\n"),
        Some(Discharge::ObserverOnly) | None => String::new(),
    }
}

fn render_wait_items(items: &[WaitItem], label: &AgentLabel, role: Role) -> String {
    let refs: Vec<&WaitItem> = items.iter().collect();
    render_wait_items_ref(&refs, label, role)
}

fn render_wait_items_ref(items: &[&WaitItem], label: &AgentLabel, role: Role) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Clank wait returned work for `{label}` ({role}). Items:\n",
        label = label.as_str(),
        role = role.as_str(),
    ));
    for item in items {
        let kind = item.kind.as_deref().unwrap_or("?");
        let plan = item.plan.as_deref().unwrap_or("?");
        let full = item
            .sha
            .as_deref()
            .or(item.finalized_at.as_deref())
            .unwrap_or("");
        let short = short_sha(full);
        // Minimal hints (`wait-output-is-a-minimal-hint`): one line
        // per item — WHO/verb + plan + short sha. The HOW (the
        // `feedback write` form, verdict meanings, promote
        // evaluation, unblock command) lives in the agent's skill
        // doc, not re-taught per wake. The 12-char sha is what the
        // agent composes `feedback write --commit` from; `clank
        // status` has the rest.
        match kind {
            "reviewer" => out.push_str(&format!("  - reviewer: review {plan} @ {short}\n")),
            "master" => {
                let next = item.next.as_deref().unwrap_or("?");
                let reason = item.reason.as_deref().unwrap_or("?");
                out.push_str(&format!("  - master: {next} {plan} @ {short} ({reason})\n"));
            }
            "finished" => out.push_str(&format!("  - finished: {plan} @ {short}\n")),
            "idle" => {
                let prompt = item.prompt.as_deref().unwrap_or("");
                out.push_str(&format!("  - idle: {prompt}\n"));
            }
            "adhoc_review" => out.push_str(&format!("  - adhoc-review: {short}\n")),
            "adhoc_revise" => out.push_str(&format!("  - adhoc-revise: {short}\n")),
            "adhoc_settled" => {
                let gate = item.gate.as_deref().unwrap_or("?");
                out.push_str(&format!("  - adhoc-settled: {short} (gate {gate})\n"));
            }
            "promote_from_queue" => {
                let name = item.name.as_deref().unwrap_or("?");
                let priority = item.priority.unwrap_or(0);
                out.push_str(&format!("  - promote: {name} (priority {priority:03})\n"));
            }
            "blocked" => {
                let agent = item.agent.as_deref().unwrap_or("?");
                let name = item.name.as_deref().unwrap_or("?");
                let scope = item.plan.as_deref().unwrap_or("repo");
                out.push_str(&format!(
                    "  - blocked: {agent}/{name} on {scope} (awaiting human)\n"
                ));
            }
            "unblocked" => {
                let name = item.name.as_deref().unwrap_or("?");
                // The answer is the action payload (the human's
                // instruction) — signal, not tutorial.
                let answer = item.answer.as_deref().unwrap_or("");
                out.push_str(&format!("  - unblocked: {name}: {answer}\n"));
            }
            "pr_reviewer" => {
                let pr = item.pr.unwrap_or(0);
                let round = item.round.unwrap_or(0);
                out.push_str(&format!("  - pr-review: review #{pr} round {round}\n"));
            }
            "pr_master" => {
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
            "multiple_plans_open" => {
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
            "github_event" => {
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
            "command_event" => {
                let name = item.name.as_deref().unwrap_or("?");
                let exit = match item.exit_code {
                    Some(c) => format!("exit {c}"),
                    None => "signaled".to_string(),
                };
                let snip = item
                    .output_tail
                    .as_deref()
                    .and_then(|t| t.lines().last())
                    .filter(|l| !l.is_empty())
                    .map(|l| format!(": {l}"))
                    .unwrap_or_default();
                out.push_str(&format!("  - command: {name} {exit}{snip}\n"));
            }
            // Its OWN variant, not a `master` reason — `work_for`
            // emits it directly. It rendered through the catchall
            // until now, which is exactly how a recurring wake ends up
            // with no explanation.
            "fix_commit_tag" => out.push_str(&format!("  - fix-commit-tag: {plan} @ {short}\n")),
            other => out.push_str(&format!("  - {other}: {plan} @ {short}\n")),
        }
        out.push_str(&discharge_suffix(kind, item.reason.as_deref()));
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The ENTRY decision, as the hook takes it: consume the marker,
    /// then judge it against a fresh task list.
    fn entry_outcome(dir: &Path, live: &[String]) -> Option<HookOutcome> {
        let attending = consume_attending(dir);
        attendance_silence(attending.as_ref(), live, 0, dir)
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
        let live = vec!["task-42".to_string()];

        let full = Attending {
            task: "task-42".into(),
            desc: None,
            pid: Some(me),
            token: token.clone(),
            correlation: Correlation::TaskId,
        };
        assert!(full.provably_live(&live, 0), "all three signals agree");

        // Each signal removed in turn — every one is load-bearing.
        assert!(
            !full.provably_live(&[], 0),
            "not listed live: the completion wake has already been spent"
        );
        assert!(
            !full.provably_live(&["other".to_string()], 0),
            "a DIFFERENT task being live says nothing about this one"
        );
        assert!(
            !Attending {
                pid: None,
                ..full.clone()
            }
            .provably_live(&live, 0),
            "no pid — the penlock marker's exact shape — cannot prove anything"
        );
        assert!(
            !Attending {
                token: None,
                ..full.clone()
            }
            .provably_live(&live, 0),
            "a bare pid is not an identity; pids are reused"
        );
        // A pid that cannot be alive, and a token that no longer holds
        // its pid, are each independently fatal.
        assert!(
            !Attending {
                pid: Some(-1),
                ..full.clone()
            }
            .provably_live(&live, 0),
            "an impossible pid is not alive"
        );
        assert!(
            !Attending {
                token: Some(crate::proc_identity::ProcToken::Unrecognised),
                ..full.clone()
            }
            .provably_live(&live, 0),
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
        let as_id = ["shared-text".to_string()];

        // Each is satisfied by its OWN evidence.
        assert!(
            manual.provably_live(&as_id, 0),
            "task id satisfies a manual marker"
        );
        assert!(
            owned.provably_live(&[], 1),
            "a live run satisfies an owned marker"
        );

        // And by nothing else, however exactly the text coincides.
        assert!(
            !manual.provably_live(&[], 1),
            "a manual marker must NOT be satisfied by a run that shares its text"
        );
        assert!(
            !owned.provably_live(&as_id, 0),
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
            // What `clank run` writes: the description, because no
            // harness id exists when the marker is written.
            task: "test run".into(),
            desc: Some("test run".into()),
            pid: Some(me),
            token: crate::proc_identity::token_for(me),
            correlation: Correlation::RunDesc,
        };

        assert!(
            rec.provably_live(&[], 1),
            "one live run with this description is the proof"
        );
        assert!(
            !rec.provably_live(&[], 0),
            "no live run: nothing shows the work is still going"
        );
        assert!(
            !rec.provably_live(&[], 2),
            "two live runs sharing a description is an ambiguity, not a proof"
        );

        // The id path is unaffected — but it belongs to a marker
        // written for it, not to this one.
        assert!(
            Attending {
                correlation: Correlation::TaskId,
                ..rec.clone()
            }
            .provably_live(&["test run".to_string()], 0)
        );

        // And correlation never substitutes for identity: an
        // unprovable marker stays unprovable however it correlates.
        assert!(
            !Attending {
                token: None,
                ..rec.clone()
            }
            .provably_live(&[], 1),
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

    /// The empty-items path returns before the marker is examined, so
    /// consumption cannot live there. This pins that the caller takes
    /// the marker first — the escape route that let markers outlive
    /// their turn.
    #[test]
    fn no_work_still_consumes_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42");
        let out = outcome(dir.path(), serde_json::json!([]), &[]);
        assert!(
            matches!(
                out,
                HookOutcome::Silent {
                    why: SilentReason::NoWork
                }
            ),
            "got {out:?}"
        );
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

    /// The DRIFT GUARD. An exhaustive match over the core enum, so
    /// adding a `WaitItem` variant without classifying it fails to
    /// compile. String matching on the hook's loose projection cannot
    /// give exhaustiveness, which is how `fix_commit_tag` reached the
    /// catchall unnoticed in the first place.
    fn wire_tag_of(item: &clank_core::wait::WaitItem) -> &'static str {
        use clank_core::wait::WaitItem as W;
        match item {
            W::Master { .. } => "master",
            W::Reviewer { .. } => "reviewer",
            W::Finished { .. } => "finished",
            W::Idle { .. } => "idle",
            W::AdHocReview { .. } => "adhoc_review",
            W::AdHocRevise { .. } => "adhoc_revise",
            W::AdHocSettled { .. } => "adhoc_settled",
            W::FixCommitTag { .. } => "fix_commit_tag",
            W::MultiplePlansOpen { .. } => "multiple_plans_open",
            W::PromoteFromQueue { .. } => "promote_from_queue",
            W::GithubEvent { .. } => "github_event",
            W::CommandEvent { .. } => "command_event",
            W::ForCommit { .. } => "for_commit",
            W::ForFinished { .. } => "for_finished",
            W::ForBlocked { .. } => "for_blocked",
            W::Blocked { .. } => "blocked",
            W::Unblocked { .. } => "unblocked",
            W::PrReviewer { .. } => "pr_reviewer",
            W::PrMaster { .. } => "pr_master",
        }
    }

    #[test]
    fn every_wait_item_variant_is_classified() {
        // Table-driven over the WHOLE algebra. A kind cannot be tested
        // into existence without being classified, and `wire_tag_of`
        // above stops a new variant slipping past the table.
        use Discharge::*;
        let master_reasons = [
            "gate_continue",
            "address_commit_changes",
            "ready_to_finalize",
            "commit_plan_revision",
        ];
        for r in master_reasons {
            assert!(
                matches!(discharge_of("master", Some(r)), Some(Until(_))),
                "master/{r} must state what ends it"
            );
        }
        let repeating = [
            "reviewer",
            "fix_commit_tag",
            "multiple_plans_open",
            "promote_from_queue",
            "github_event",
            "adhoc_review",
            "adhoc_revise",
            "pr_reviewer",
            "pr_master",
            "unblocked",
            "blocked",
            "idle",
            // Re-armed command sources re-fire; see `discharge_of`.
            "command_event",
        ];
        for k in repeating {
            assert!(
                matches!(discharge_of(k, None), Some(Until(_))),
                "{k} repeats and must state what ends it"
            );
        }
        for k in ["finished", "adhoc_settled"] {
            assert!(
                matches!(discharge_of(k, None), Some(OneShot(_))),
                "{k} is one-shot and must say so rather than stay silent"
            );
        }
        for k in ["for_commit", "for_finished", "for_blocked"] {
            assert_eq!(
                discharge_of(k, None),
                Some(ObserverOnly),
                "{k} is watcher delivery, classified rather than skipped"
            );
        }
    }

    #[test]
    fn master_keys_on_the_reason_not_the_kind() {
        // Four reasons, four different discharges. Keying on the kind
        // alone would print a condition that does not apply — worse
        // than silence, because it reads as authoritative.
        let cont = discharge_of("master", Some("gate_continue"));
        let revise = discharge_of("master", Some("address_commit_changes"));
        assert_ne!(cont, revise);
        assert!(
            matches!(cont, Some(Discharge::Until(t)) if t.contains("gate")),
            "continue is discharged by moving the GATE: {cont:?}"
        );
        assert!(
            matches!(revise, Some(Discharge::Until(t)) if t.contains("feedback")),
            "revise is discharged by addressing the FEEDBACK: {revise:?}"
        );
        // A future reason must not be given a borrowed condition.
        assert_eq!(discharge_of("master", Some("some_new_reason")), None);
    }

    #[test]
    fn blocked_tells_the_agent_not_to_act() {
        // The one wake where acting is WRONG, and where an agent is
        // most likely to invent an action to stop the nagging.
        let Some(Discharge::Until(t)) = discharge_of("blocked", None) else {
            panic!("blocked must carry guidance");
        };
        assert!(t.contains("CANNOT"), "{t}");
        assert!(t.contains("human"), "{t}");
        assert!(t.contains("not answer it yourself"), "{t}");
    }

    #[test]
    fn unknown_kinds_render_without_a_condition_and_do_not_panic() {
        assert_eq!(discharge_of("some_future_kind", None), None);
        assert!(discharge_suffix("some_future_kind", None).is_empty());
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
    async fn the_deadline_re_arms_except_where_a_continuation_would_spin() {
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
        assert!(
            matches!(
                got,
                Err(HookOutcome::Silent {
                    why: SilentReason::PollDeadline
                })
            ),
            "opencode injects on session.idle, so a continuation spins (codex 8000d6e)"
        );
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

        let got = await_wait_under_deadline(past, DeadlineAction::Sleep, || {
            Ok(std::future::pending::<std::io::Result<std::process::Output>>())
        })
        .await;
        assert!(
            matches!(
                got,
                Err(HookOutcome::Silent {
                    why: SilentReason::PollDeadline
                })
            ),
            "the sleeping adapters keep their old pre-check behaviour"
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

    /// Adding an adapter must force an answer rather than inherit one.
    #[test]
    fn every_adapter_answers_the_deadline_question() {
        assert_eq!(deadline_action(Tool::Claude), DeadlineAction::ReArm);
        assert_eq!(deadline_action(Tool::Codex), DeadlineAction::ReArm);
        assert_eq!(deadline_action(Tool::OpenCode), DeadlineAction::Sleep);
        assert_eq!(deadline_action(Tool::Grok), DeadlineAction::Sleep);
    }

    #[test]
    fn park_decision_matrix_and_generation_reads() {
        // claude-asyncrewake-work-loop: takeover is deterministic —
        // free parks, a live same-generation waiter defers, a stale
        // holder is awaited (it self-releases on its 2s check), and
        // the wake guard suppresses a stale emission.
        assert_eq!(park_decision(true, None, 3), ParkDecision::Park);
        assert_eq!(
            park_decision(false, Some(3), 3),
            ParkDecision::DeferToLiveWaiter
        );
        assert_eq!(
            park_decision(false, Some(2), 3),
            ParkDecision::AwaitStaleHandoff,
            "older holder is stale"
        );
        assert_eq!(
            park_decision(false, None, 3),
            ParkDecision::AwaitStaleHandoff,
            "unreadable holder is treated stale, not adopted"
        );

        let dir = tempfile::tempdir().unwrap();
        // Absent / garbage read as generation 0 (pre-generation
        // installs keep working; SessionStart mints real ones).
        assert_eq!(read_generation(dir.path()), 0);
        std::fs::write(dir.path().join("wait.gen"), "not-a-number").unwrap();
        assert_eq!(read_generation(dir.path()), 0);
        std::fs::write(dir.path().join("wait.gen"), "7\n").unwrap();
        assert_eq!(read_generation(dir.path()), 7);

        // Binding mints (the fresh-session claim — SessionStart
        // fires before the bind and cannot cover it, M2 e2e): a
        // stale waiter of gen 7 is revoked by the successor's bind.
        assert_eq!(
            crate::agent_store::mint_wait_generation(dir.path()).unwrap(),
            8
        );
        assert_eq!(read_generation(dir.path()), 8);

        // The lease is exclusive within a process and frees on drop
        // (same flock mechanism as the event WAL).
        let a = try_wait_lease(dir.path()).unwrap();
        assert!(a.is_some());
        assert!(try_wait_lease(dir.path()).unwrap().is_none());
        drop(a);
        // NOT instantaneous under concurrent forks: an `flock` lives on
        // the open file DESCRIPTION, `fork()` duplicates the fd into the
        // child, and `O_CLOEXEC` closes it only at `exec()`. So while any
        // other test in this binary spawns a process, a dropped holder's
        // lock can still be held by that child for the fork→exec window.
        // The property under test is that the drop RELEASES, not that it
        // releases within one instruction.
        let freed = (0..200).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            try_wait_lease(dir.path()).unwrap().is_some()
        });
        assert!(freed, "dropping the holder frees the lease");
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
    async fn armed_background_wait_yields_silently_before_clank_resolution() {
        // An armed background `clank wait` yields silently — the branch
        // precedes all clank resolution, so it can't surface a Diagnostic.
        let dir = init_repo();
        let repo = dir.path();
        bind(repo, "codex", "sess-armed");

        let input: HookInput = serde_json::from_value(serde_json::json!({
            "session_id": "sess-armed",
            "cwd": "/tmp",
            "stop_hook_active": false,
            "background_tasks": [
                {"id": "t1", "status": "running", "command": "cd /tmp; clank wait --author codex"}
            ],
        }))
        .unwrap();
        let outcome = compute_outcome(Tool::Claude, Some(repo), input).await;
        assert_eq!(
            outcome,
            HookOutcome::Silent {
                why: SilentReason::YieldArmed
            }
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
                {
                    "kind": "command_event",
                    "name": "signal",
                    "exit_code": null,
                    "output_tail": "line one\nlast line"
                },
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
            out.contains("  - command: signal signaled: last line"),
            "command hint carries name/exit/tail snippet: {out}"
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

    #[test]
    fn nudge_states_the_count_and_never_echoes_commands() {
        // lloyd (dark-skippy): real background commands are multi-clause
        // shell one-liners; echoing them buried the instruction. The nudge
        // states only THAT something is running (and how many) — no
        // command text ever reaches the agent.
        let monster = "until [ -s /tmp/x.output ]; do sleep 3; done; grep -iE \"FILL|error\" /tmp/x.output | head -20";
        let input: HookInput = serde_json::from_value(serde_json::json!({
            "session_id": "sess-nudge-test",
            "cwd": "/tmp",
            "stop_hook_active": false,
            "background_tasks": [
                {"id": "t1", "status": "running", "command": monster},
                {"id": "t2", "status": "running", "command": monster},
                {"id": "t3", "status": "running", "command": "cd /tmp && clank wait --author codex"},
            ],
        }))
        .unwrap();
        let reason = nudge_reason(&input);
        assert!(
            reason.contains("2 background tasks"),
            "counts non-clank-wait tasks: {reason}"
        );
        assert!(
            !reason.contains("until") && !reason.contains("/tmp/x.output"),
            "no command text leaks: {reason}"
        );
        // The instruction core: command + the run_in_background detail.
        assert!(reason.contains("run `clank wait` (run_in_background: true)"));
        assert!(reason.contains("run_in_background: true"));
        assert!(!reason.contains("Bash tool"), "terse: no tool tutorial");

        // Singular wording for one task.
        let one: HookInput = serde_json::from_value(serde_json::json!({
            "session_id": "sess-nudge-test",
            "cwd": "/tmp",
            "stop_hook_active": false,
            "background_tasks": [{"id": "t1", "status": "running", "command": "cargo build"}],
        }))
        .unwrap();
        let reason = nudge_reason(&one);
        assert!(
            reason.contains("a background task still running"),
            "{reason}"
        );
        assert!(!reason.contains("cargo"), "no command text: {reason}");
    }

    #[tokio::test]
    async fn claude_idle_nudges_to_arm_wait_and_never_renders_items() {
        // claude-stop-hook-minimal-hint: on claude the idle branch blocks
        // with the arm-the-wait hint and NEVER runs the in-hook wait —
        // even when work exists RIGHT NOW (here: a dirty plan file the
        // master must commit, which a wait would return immediately).
        // The armed wait's completion wake is what delivers the items.
        use crate::cli::teams_config::{AgentDescription, RosterRole};
        let dir = init_repo();
        let repo = dir.path();
        let label = bind(repo, "claude", "sess-claude-idle");
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
        std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
        std::fs::write(repo.join(".clank/plans/some-plan.md"), "# some-plan\n").unwrap();

        let input = hook_input("sess-claude-idle", None);
        let outcome = compute_outcome(Tool::Claude, Some(repo), input).await;
        let HookOutcome::Continue { reason } = &outcome else {
            panic!("expected the arm-the-wait Continue, got {outcome:?}");
        };
        assert!(
            reason.contains("Nothing is watching for clank work"),
            "{reason}"
        );
        assert!(
            reason.contains("run `clank wait` (run_in_background: true)"),
            "{reason}"
        );
        assert!(
            !reason.contains("some-plan") && !reason.contains("  - "),
            "the hint must carry no work items: {reason}"
        );
    }

    #[test]
    fn idle_nudge_keeps_the_instruction_without_a_background_preamble() {
        let input: HookInput = serde_json::from_value(serde_json::json!({
            "session_id": "sess-idle-nudge",
            "cwd": "/tmp",
            "stop_hook_active": false,
            "background_tasks": [],
        }))
        .unwrap();
        let reason = nudge_reason(&input);
        assert!(
            reason.contains("Nothing is watching for clank work"),
            "{reason}"
        );
        assert!(
            reason.contains("run `clank wait` (run_in_background: true)"),
            "{reason}"
        );
        assert!(reason.contains("run_in_background: true"), "{reason}");
        assert!(!reason.contains("Bash tool"), "terse: {reason}");
        assert!(
            !reason.contains("still running"),
            "no background-task preamble when idle: {reason}"
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
    fn adhoc_settled_renders_the_verdict_one_liner() {
        // The settle wake (adhoc-continue-wake-investigation): short
        // sha + verdict, nothing else — there is no follow-on work
        // command to teach.
        let out = items_text(&[
            serde_json::json!({"kind": "adhoc_settled", "sha": FULL_SHA, "gate": "continued"}),
            serde_json::json!({"kind": "adhoc_settled", "sha": FULL_SHA, "gate": "finished"}),
        ]);
        let short = short_sha(FULL_SHA);
        assert!(out.contains(&format!("  - adhoc-settled: {short} (gate continued)\n")));
        assert!(out.contains(&format!("  - adhoc-settled: {short} (gate finished)\n")));
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
        assert_eq!(items[0].kind.as_deref(), Some("reviewer"));
        assert_eq!(items[1].next.as_deref(), Some("revise"));
    }

    #[test]
    fn an_idle_poll_no_longer_self_expires() {
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

        // Idle is now expressed the only way left: exit 0, no items.
        let idle = outcome_from_wait_output(&out(0, r#"{"items":[]}"#), &label, Role::Master, &[]);
        assert!(
            matches!(
                idle,
                HookOutcome::Silent {
                    why: SilentReason::NoWork
                }
            ),
            "idle is exit 0 with no items, got {idle:?}"
        );
    }

    #[test]
    fn the_in_hook_wait_argv_carries_the_ownership_flag() {
        let argv = wait_argv(Path::new("/repo"), &AgentLabel::parse("codex").unwrap());
        assert!(
            argv.iter().any(|a| a == "--die-with-owner"),
            "without this the in-hook wait has no bound at all: {argv:?}"
        );
        // Absence assertions over a flag the CLI no longer accepts.
        // These are reachable, not ceremony: this inspects what the
        // BUILDER produces, before clap sees it, so re-adding either
        // flag here fails the test. And it must — a hook injecting a
        // flag `clank wait` rejects would break the wait outright, at
        // runtime, in production.
        assert!(
            !argv.iter().any(|a| a == "--timeout"),
            "the flag is gone: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "--role"),
            "role is derived from identity; the hook must not pin one: {argv:?}"
        );
    }

    #[tokio::test]
    async fn owned_wait_refuses_a_child_with_no_ownership_pipe() {
        // The degradation codex flagged: `stdin.take()` yielding None
        // must be a hard error, not a silently unowned wait that
        // outlives a killed hook.
        let mut bare = Command::new("true");
        bare.stdin(Stdio::null());
        let err = match OwnedWait::spawn(&mut bare) {
            Err(e) => e,
            Ok(_) => panic!("a child with no ownership pipe must be refused"),
        };
        assert!(err.contains("ownership"), "explains the refusal: {err}");

        // And the real builder DOES pipe stdin, so construction works.
        let owned = OwnedWait::spawn(&mut wait_command(
            Path::new("true"),
            Path::new("/repo"),
            &AgentLabel::parse("codex").unwrap(),
        ))
        .expect("the hook's own command must carry the pipe");
        let _ = owned.run().await;
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
}
