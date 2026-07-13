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
//! The two tools get different work-delivery models
//! (claude-stop-hook-minimal-hint): **claude** never waits in-hook and
//! never renders work items — its only continuation is the
//! self-extinguishing arm-the-wait hint, and
//! the armed wait's completion wake delivers the work (claude wakes the
//! session when a background task finishes). **codex** has no such wake
//! channel, so its hook long-polls `clank wait` in-hook and blocks with
//! the rendered items.
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
    let tool: Tool = args.tool.into();
    let outcome = match read_hook_stdin() {
        Ok(input) => compute_outcome(tool, args.repo.as_deref(), input).await,
        Err(e) => HookOutcome::Diagnostic { message: e },
    };
    emit_and_exit(outcome, tool);
}

async fn compute_outcome(
    tool: Tool,
    repo_override: Option<&Path>,
    input: HookInput,
) -> HookOutcome {
    // Decide how the agent's in-flight background work affects this
    // turn-end, BEFORE any clank resolution (`background_disposition` is a
    // pure function of the turn). `YieldArmed` never runs a wait — Claude
    // Code auto-wakes when the work completes, and running our own wait here
    // would block that wake. Only `NeedsWorkCheck` / `NoBackgroundWork` fall
    // through to resolution, so a config Diagnostic can only surface when we
    // were going to engage clank anyway.
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
            _ => match tool {
                // Claude never waits in-hook and never renders work items —
                // the hook's only continuation is the arm-the-wait hint, and
                // the armed wait's completion wake delivers the work. Work
                // presence is deliberately NOT consulted: if work exists the
                // armed wait exits immediately and the wake carries it
                // (claude-stop-hook-minimal-hint).
                Tool::Claude => HookOutcome::Continue {
                    reason: nudge_reason(&input),
                },
                // Codex has no background-task wake channel, so the in-hook
                // long-poll + block-with-items model stays.
                Tool::Codex => {
                    let wait_timeout = cfg.as_ref().and_then(|c| c.wait_timeout.clone());
                    compute_wait_outcome(&repo, &label, role, wait_timeout.as_deref()).await
                }
                // Grok's hooks are PASSIVE (grok-first-class): clank
                // installs no grok adapter and no continuation could
                // drive it. If something wires this up anyway, say so.
                Tool::Grok => HookOutcome::Diagnostic {
                    message: "hook: grok has no stop-hook adapter (grok hooks are passive)".into(),
                },
            },
        },
    }
}

/// Non-blocking peek: does `label` have actionable clank work right now?
/// Self-spawns `clank wait --peek --json` (side-effect-free: fires no
/// hooks) and CAPTURES its stdout — never inherits it — so the peek's JSON
/// envelope can't corrupt the hook's own protocol stdout (ruthless c042912).
async fn peek_has_work(repo: &Path, label: &AgentLabel, role: Role) -> Result<bool, String> {
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
        Some(0) => parse_wait_json(&output.stdout)
            .map(|items| !items.is_empty())
            .map_err(|e| format!("peek stdout malformed: {e}")),
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

/// The `--timeout` the codex in-hook poll runs with. An explicit
/// per-agent `wait_timeout` wins; the DEFAULT expires five minutes
/// BEFORE codex's hook-runner ceiling (setup's [`HOOK_TIMEOUT_SECS`],
/// one shared constant so they can't drift) — a poll that outlives
/// the ceiling gets KILLED and codex banners `Stop hook (failed):
/// hook timed out` for a perfectly normal idle
/// (codex-poll-expires-cleanly). The clean expiry exits 2, which maps
/// to Silent below.
fn codex_wait_timeout(explicit: Option<&str>) -> String {
    match explicit {
        Some(t) => t.to_string(),
        None => format!("{}s", crate::cli::setup::HOOK_TIMEOUT_SECS - 300),
    }
}

/// Long-poll via a self-spawned `clank wait --json`. CODEX-ONLY:
/// claude's hook never waits in-hook (see the module doc).
/// Reuses the watcher loop without refactoring it. On items →
/// Continue. On timeout (exit 2) → Silent. Anything else →
/// Diagnostic.
///
/// Self-spawning has one advantage over factoring out the loop:
/// the wait process is a clean child that gets killed if the
/// agent kills the hook (Stdio::piped + drop kills the child).
async fn compute_wait_outcome(
    repo: &Path,
    label: &AgentLabel,
    role: Role,
    wait_timeout: Option<&str>,
) -> HookOutcome {
    use tokio::process::Command;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return HookOutcome::Diagnostic {
                message: format!("hook: current_exe failed: {e}"),
            };
        }
    };

    let role_arg = match role {
        Role::Master => "master",
        Role::Reviewer => "reviewer",
    };
    let timeout_arg = codex_wait_timeout(wait_timeout);

    let mut cmd = Command::new(&exe);
    cmd.arg("wait")
        .arg("--repo")
        .arg(repo)
        .arg("--author")
        .arg(label.as_str())
        .arg("--role")
        .arg(role_arg)
        .arg("--timeout")
        .arg(timeout_arg)
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            return HookOutcome::Diagnostic {
                message: format!("hook: spawning wait failed: {e}"),
            };
        }
    };

    match output.status.code() {
        Some(0) => match parse_wait_json(&output.stdout) {
            Ok(items) if items.is_empty() => HookOutcome::Silent {
                why: SilentReason::NoWork,
            },
            Ok(items) => HookOutcome::Continue {
                reason: render_wait_items(&items, label, role),
            },
            Err(e) => HookOutcome::Diagnostic {
                message: format!("hook: wait stdout malformed: {e}"),
            },
        },
        Some(2) => HookOutcome::Silent {
            why: SilentReason::WaitTimeout,
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
struct WaitItem {
    kind: Option<String>,
    plan: Option<String>,
    sha: Option<String>,
    finalized_at: Option<String>,
    next: Option<String>,
    reason: Option<String>,
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
        let label = AgentLabel::parse(label).unwrap();
        let sid = clank_core::ids::SessionId::parse(session).unwrap();
        crate::agent_store::bind_session_to_agent(repo, &label, Tool::Claude, &sid).unwrap();
        label
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
    fn codex_wait_default_expires_before_the_hook_ceiling() {
        // codex-poll-expires-cleanly: with wait_timeout unset the poll
        // must exit (2 → Silent) BEFORE codex's hook runner kills it at
        // setup's ceiling — the failed banner for a normal idle. One
        // shared constant so the two can't drift.
        let default = codex_wait_timeout(None);
        let secs: u64 = default
            .strip_suffix('s')
            .expect("seconds-suffixed duration")
            .parse()
            .expect("numeric");
        assert!(
            secs < crate::cli::setup::HOOK_TIMEOUT_SECS,
            "default {secs}s must undercut the {} ceiling",
            crate::cli::setup::HOOK_TIMEOUT_SECS
        );
        assert_eq!(secs, crate::cli::setup::HOOK_TIMEOUT_SECS - 300);
        // An explicit per-agent value passes through untouched.
        assert_eq!(codex_wait_timeout(Some("4h")), "4h");
        assert_eq!(codex_wait_timeout(Some("0")), "0");
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
