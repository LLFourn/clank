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

    // Role is roster-derived. The hook path is fail-soft: if the
    // repo has no master configured (or resolution errors),
    // default to Reviewer —
    // a misconfigured repo shouldn't block the agent's session,
    // and Reviewer is the conservative default (won't spuriously
    // drive master-only actions).
    let role = crate::agent_store::resolve_role(&repo, &label)
        .unwrap_or_else(|_| clank_core::vocab::Role::default());

    match effective {
        AutoMode::Off => HookOutcome::Silent {
            why: SilentReason::AutoOff,
        },
        AutoMode::On => match disposition {
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
                return asyncrewake_park(&repo, &label, role, started, &live_ids).await;
            }
            BgDisposition::NeedsWorkCheck => match peek_has_work(&repo, &label, role).await {
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
                return asyncrewake_park(&repo, &label, role, started, &live_ids).await;
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
                    compute_wait_outcome(&repo, &label, role, started, &live_ids).await
                }
                // Grok's hooks are PASSIVE (grok-first-class): clank
                // installs no grok adapter and no continuation could
                // drive it. If something wires this up anyway, say so.
                LoopPolicy::Passive => HookOutcome::Diagnostic {
                    message: "hook: grok has no stop-hook adapter (grok hooks are passive)".into(),
                },
            },
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
    let role = crate::agent_store::resolve_role(&repo, &label)
        .unwrap_or_else(|_| clank_core::vocab::Role::default());
    let Ok(items) = peek_items(&repo, &label, role).await else {
        return Ok(());
    };
    if items.is_empty() {
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
        o = compute_wait_outcome(repo, label, role, started, live_ids) => o,
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
async fn peek_has_work(repo: &Path, label: &AgentLabel, role: Role) -> Result<bool, String> {
    peek_items(repo, label, role).await.map(|i| !i.is_empty())
}

/// The peek's ITEMS — shared by the work-check and SessionStart's
/// catch-up context (claude-asyncrewake-work-loop).
pub(crate) async fn peek_items(
    repo: &Path,
    label: &AgentLabel,
    role: Role,
) -> Result<Vec<WaitItem>, String> {
    use tokio::process::Command;

    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let role_arg = match role {
        Role::Master => "master",
        Role::Reviewer => "reviewer",
    };
    let output = Command::new(&exe)
        .arg("wait")
        .arg("--peek")
        .arg("--repo")
        .arg(repo)
        .arg("--author")
        .arg(label.as_str())
        .arg("--role")
        .arg(role_arg)
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
fn wait_argv(repo: &Path, label: &AgentLabel, role: Role) -> Vec<String> {
    let role_arg = match role {
        Role::Master => "master",
        Role::Reviewer => "reviewer",
    };
    vec![
        "wait".into(),
        "--repo".into(),
        repo.display().to_string(),
        "--author".into(),
        label.as_str().to_string(),
        "--role".into(),
        role_arg.into(),
        // Ownership, not decoration: without this the wait has no
        // bound at all and outlives a killed hook forever.
        "--die-with-owner".into(),
        "--json".into(),
    ]
}

fn wait_command(exe: &Path, repo: &Path, label: &AgentLabel, role: Role) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(wait_argv(repo, label, role))
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
) -> HookOutcome {
    // Fixed at hook entry, so everything below — current_exe, command
    // construction, the child spawn — is charged against it.
    let deadline = poll_deadline(started);
    if std::time::Instant::now() >= deadline {
        return HookOutcome::Silent {
            why: SilentReason::PollDeadline,
        };
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return HookOutcome::Diagnostic {
                message: format!("hook: current_exe failed: {e}"),
            };
        }
    };

    let mut cmd = wait_command(&exe, repo, label, role);

    let owned = match OwnedWait::spawn(&mut cmd) {
        Ok(w) => w,
        Err(e) => return HookOutcome::Diagnostic { message: e },
    };

    // Expire BELOW the runner's ceiling, cleanly. Dropping `owned`
    // closes the stdin pipe the wait treats as its owner sentinel, so
    // the child reaps itself — the deadline does not leak a wait, and
    // it does not mask owner death: whichever fires first ends the
    // same way.
    // Re-checked here by construction: the deadline is absolute, so
    // the spawn above has already eaten into it.
    let output = match under_deadline(owned.run(), deadline).await {
        None => {
            return HookOutcome::Silent {
                why: SilentReason::PollDeadline,
            };
        }
        Some(Ok(o)) => o,
        Some(Err(e)) => {
            return HookOutcome::Diagnostic {
                message: format!("hook: waiting on wait failed: {e}"),
            };
        }
    };

    let agent_dir = crate::agent_store::agents_root(repo).join(label.as_str());
    outcome_from_wait_output(&output, label, role, &agent_dir, live_ids)
}

// ── attending a background task (attending-suppresses-standing-wakes) ──

/// Where the agent records the background task it is waiting on.
fn attending_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("attending")
}

/// What the agent acknowledged when it started attending: the live
/// background task, AND the exact standing item it had already seen.
///
/// The item identity is load-bearing. A reviewer's CONTINUE on a NEW
/// commit surfaces as the SAME `gate_continue` reason at a new sha
/// (`wait.rs:1251-1258`), so suppressing by reason alone would discard
/// genuine review progress. Only an exact (plan, sha) match is
/// suppressed; anything else is news and wakes.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attending {
    pub(crate) task: String,
    /// The acknowledged standing item, absent if none was pending when
    /// attendance began — in which case nothing is suppressed.
    #[serde(default)]
    pub(crate) plan: Option<String>,
    #[serde(default)]
    pub(crate) sha: Option<String>,
}

/// The attendance record, iff its task is still LIVE.
///
/// Ownership by liveness, never by claim: the recorded id is validated
/// against the tool's own live task list, and an id absent from it is
/// void and deleted. The agent therefore cannot wedge the hook silent
/// by forgetting to clear the marker — a stale "I am busy" flag would
/// cause MISSED wakes, which is strictly worse than the noise this
/// suppresses.
fn attending_live_task(agent_dir: &Path, live_ids: &[String]) -> Option<Attending> {
    let path = attending_path(agent_dir);
    let raw = std::fs::read_to_string(&path).ok()?;
    let Ok(rec) = serde_json::from_str::<Attending>(&raw) else {
        // Unreadable marker suppresses nothing — fail toward NOISE,
        // never toward silence.
        let _ = std::fs::remove_file(&path);
        return None;
    };
    if live_ids.iter().any(|id| id == &rec.task) {
        return Some(rec);
    }
    let _ = std::fs::remove_file(&path);
    None
}

impl Attending {
    /// Is this item EXACTLY the standing work already acknowledged?
    ///
    /// Reason, plan and sha must all match. A new sha is review
    /// progress and must wake, even though its reason is identical.
    fn acknowledges(&self, item: &WaitItem) -> bool {
        is_standing_work(item)
            && self.plan.is_some()
            && self.plan == item.plan
            && self.sha.is_some()
            && self.sha == item.sha
    }
}

/// Is this item work the agent is ALREADY doing?
///
/// Exactly one reason qualifies: `GateContinue` on a master item —
/// "latest reviewable commit is CONTINUE; master keeps working", which
/// stays true for the whole implementation and is information the
/// agent already has.
///
/// `Revise` is deliberately NOT here. It carries
/// `AddressCommitChanges` — newly arrived REQUEST_CHANGES — which is
/// news, and suppressing it would mean sitting through reviewer
/// feedback for the length of a build.
fn is_standing_work(item: &WaitItem) -> bool {
    // Stringly-typed because this crosses the `wait --json` subprocess
    // boundary, where the hook's projection is deliberately loose. A
    // test pins these against what core actually serializes, so a
    // rename in `WaitingReason` cannot silently stop suppressing.
    item.kind.as_deref() == Some("master")
        && item.reason.as_deref() == Some(clank_core::vocab::WaitingReason::GateContinue.as_str())
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
    agent_dir: &Path,
    live_ids: &[String],
) -> HookOutcome {
    match output.status.code() {
        Some(0) => match parse_wait_json(&output.stdout) {
            Ok(items) if items.is_empty() => HookOutcome::Silent {
                why: SilentReason::NoWork,
            },
            Ok(items) => {
                let attending = attending_live_task(agent_dir, live_ids);
                let kept: Vec<&WaitItem> = items
                    .iter()
                    .filter(|i| !attending.as_ref().is_some_and(|a| a.acknowledges(i)))
                    .collect();
                if kept.is_empty() {
                    return HookOutcome::Silent {
                        why: SilentReason::AttendingBackgroundTask,
                    };
                }
                let mut reason = render_wait_items_ref(&kept, label, role);
                // Teach the protocol exactly where it would otherwise
                // loop: a standing item, a live task, and no marker is
                // the shape that produced a dozen identical wakes.
                if attending.is_none() && !live_ids.is_empty() && items.iter().any(is_standing_work)
                {
                    reason.push_str(&format!(
                        "\n\nYou have a live background task ({}). If you are WAITING on it, \
                         record it — `clank attending {}` — and this will stop waking you with \
                         work you are already doing. Do not end your turn to poll it.",
                        live_ids.join(", "),
                        live_ids.first().map(String::as_str).unwrap_or("<task-id>"),
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
            other => out.push_str(&format!("  - {other}: {plan} @ {short}\n")),
        }
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

    fn write_attending(dir: &Path, task: &str, sha: Option<&str>) {
        let rec = Attending {
            task: task.to_string(),
            plan: sha.map(|_| "p".to_string()),
            sha: sha.map(str::to_string),
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

    #[test]
    fn the_suppressed_reason_matches_what_core_serializes() {
        // The predicate compares strings across the `wait --json`
        // subprocess boundary. Pinning it against core means renaming
        // the variant breaks a test rather than silently ending
        // suppression.
        assert_eq!(
            clank_core::vocab::WaitingReason::GateContinue.as_str(),
            "gate_continue"
        );
        let items: Vec<WaitItem> =
            serde_json::from_value(serde_json::json!([standing_item()])).unwrap();
        assert!(is_standing_work(&items[0]));
    }

    #[test]
    fn review_feedback_is_news_and_is_never_standing() {
        // `Revise` carries AddressCommitChanges — newly arrived
        // REQUEST_CHANGES. Suppressing it would mean sitting through
        // reviewer feedback for the length of a build.
        let items: Vec<WaitItem> = serde_json::from_value(serde_json::json!([
            {"kind":"master","plan":"p","sha":FULL_SHA,
             "next":"revise","reason":"address_commit_changes","gate":"changes_requested"}
        ]))
        .unwrap();
        assert!(!is_standing_work(&items[0]));
    }

    #[test]
    fn attending_a_live_task_suppresses_standing_work() {
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42", Some(FULL_SHA));
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
            &["task-42".to_string()],
        );
        assert!(
            matches!(
                out,
                HookOutcome::Silent {
                    why: SilentReason::AttendingBackgroundTask
                }
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn a_new_sha_wakes_even_though_the_reason_is_identical() {
        // The hole codex found: a reviewer's CONTINUE on a NEW commit
        // reuses `gate_continue` at a new sha, so suppressing by reason
        // alone silently discards real review progress. Attendance is
        // bound to the acknowledged ITEM, not just the task.
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42", Some(FULL_SHA));
        let other_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        let same = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
            &["task-42".to_string()],
        );
        assert!(
            matches!(
                same,
                HookOutcome::Silent {
                    why: SilentReason::AttendingBackgroundTask
                }
            ),
            "the acknowledged sha stays suppressed: {same:?}"
        );

        let moved = outcome_from_wait_output(
            &wait_output(serde_json::json!([
                {"kind":"master","plan":"p","sha":other_sha,
                 "next":"continue","reason":"gate_continue","gate":"continued"}
            ])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
            &["task-42".to_string()],
        );
        assert!(
            matches!(moved, HookOutcome::Continue { .. }),
            "review progress at a NEW sha must wake: {moved:?}"
        );
    }

    #[test]
    fn a_marker_with_no_acknowledged_item_suppresses_nothing() {
        // Attendance recorded while nothing was standing must not
        // become a blanket mute for whatever arrives later.
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42", None);
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
            &["task-42".to_string()],
        );
        assert!(matches!(out, HookOutcome::Continue { .. }), "got {out:?}");
    }

    #[test]
    fn an_unreadable_marker_fails_toward_noise() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("attending"), "not json").unwrap();
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
            &["task-42".to_string()],
        );
        assert!(
            matches!(out, HookOutcome::Continue { .. }),
            "a corrupt marker must never silence: {out:?}"
        );
    }

    #[test]
    fn attending_does_not_suppress_news() {
        // The guard against trading a noisy loop for silently sitting
        // through a REQUEST_CHANGES.
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42", Some(FULL_SHA));
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([
                standing_item(),
                {"kind":"unblocked","name":"q","answer":"yes"}
            ])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
            &["task-42".to_string()],
        );
        match out {
            HookOutcome::Continue { reason } => {
                assert!(reason.contains("unblocked"), "news must wake: {reason}");
                assert!(
                    !reason.contains("gate_continue"),
                    "standing work stays suppressed: {reason}"
                );
            }
            other => panic!("news must wake, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_marker_is_void_and_removed() {
        // The missed-wake failure mode, tested directly: a marker whose
        // task is gone must NOT silence the hook, and must not linger.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("attending");
        write_attending(dir.path(), "task-gone", Some(FULL_SHA));
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
            &["task-other".to_string()],
        );
        assert!(
            matches!(out, HookOutcome::Continue { .. }),
            "a stale marker must never suppress: {out:?}"
        );
        assert!(!marker.exists(), "and it is discarded, not left to rot");
    }

    #[test]
    fn no_marker_plus_a_live_task_carries_the_hint() {
        // Teaches the protocol exactly where it would otherwise loop.
        let dir = tempfile::tempdir().unwrap();
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
            &["task-42".to_string()],
        );
        match out {
            HookOutcome::Continue { reason } => {
                assert!(reason.contains("task-42"), "names the live task: {reason}");
                assert!(
                    reason.contains("clank attending"),
                    "gives the recipe: {reason}"
                );
            }
            other => panic!("no marker means it still wakes, got {other:?}"),
        }
    }

    #[test]
    fn no_background_task_means_no_hint_and_no_suppression() {
        let dir = tempfile::tempdir().unwrap();
        write_attending(dir.path(), "task-42", Some(FULL_SHA));
        let out = outcome_from_wait_output(
            &wait_output(serde_json::json!([standing_item()])),
            &AgentLabel::parse("claude").unwrap(),
            Role::Master,
            dir.path(),
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

    #[test]
    fn expiry_is_silent_and_distinct_from_emptiness() {
        // The user-visible bug was an idle looking like a FAILURE, so
        // expiry must stay on the silent wire — and stay
        // distinguishable from "there was nothing to wait for".
        assert!(!matches!(
            HookOutcome::Silent {
                why: SilentReason::PollDeadline
            },
            HookOutcome::Diagnostic { .. }
        ));
        assert_ne!(SilentReason::PollDeadline, SilentReason::NoWork);
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
        let two =
            outcome_from_wait_output(&out(2, ""), &label, Role::Master, Path::new("/nope"), &[]);
        assert!(
            matches!(two, HookOutcome::Diagnostic { .. }),
            "a non-zero wait exit must surface, got {two:?}"
        );

        // Idle is now expressed the only way left: exit 0, no items.
        let idle = outcome_from_wait_output(
            &out(0, r#"{"items":[]}"#),
            &label,
            Role::Master,
            Path::new("/nope"),
            &[],
        );
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
        let argv = wait_argv(
            Path::new("/repo"),
            &AgentLabel::parse("codex").unwrap(),
            Role::Master,
        );
        assert!(
            argv.iter().any(|a| a == "--die-with-owner"),
            "without this the in-hook wait has no bound at all: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "--timeout"),
            "the flag is gone: {argv:?}"
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
            Role::Master,
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
