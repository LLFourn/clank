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
    let mut trace = Trace::new(tool);
    let outcome = match read_hook_stdin() {
        Ok(input) => {
            trace.record_input(&input);
            compute_outcome(tool, args.repo.as_deref(), input, &mut trace).await
        }
        Err(e) => HookOutcome::Diagnostic { message: e },
    };
    // Best-effort, never outcome-changing: the trace is diagnostics.
    trace.finalize(&outcome);
    emit_and_exit(outcome, tool);
}

/// The stop-hook decision trace (stop-hook-decision-trace): one JSON
/// record OVERWRITTEN on every invocation, so "the hook fired and chose
/// not to wait" is distinguishable from "the hook never fired" — and
/// WHICH silent branch fired is recorded, not re-derived.
///
/// Written to `.clank/agents/<label>/stop-hook.json` once the session
/// resolves to an agent, else `.clank/stop-hook.json` at the repo level
/// (fired-but-unidentified; final records only — see
/// [`Trace::mark_in_flight`]). Two-phase at the AGENT path: an
/// `in_flight` record lands after identity resolves, right before the
/// slow peek/wait work, and the final decision overwrites it — a record
/// left `in_flight` means the hook died mid-decision.
///
/// Every write is BEST-EFFORT: a failed trace write must never change
/// the hook's outcome or exit code.
struct Trace {
    tool: Tool,
    session_id: Option<String>,
    cwd: Option<String>,
    repo: Option<PathBuf>,
    label: Option<String>,
    last_assistant_message: Option<String>,
    stop_hook_active: Option<bool>,
    background_tasks: usize,
    background_clank_wait: bool,
    effective_auto: Option<AutoMode>,
    role: Option<Role>,
    disposition: Option<&'static str>,
}

/// Cap on captured text (the last assistant message, continue reasons):
/// enough to recognize the turn, small enough to never matter on disk.
const TRACE_TEXT_CAP: usize = 2000;

fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(cap).collect();
        out.push('…');
        out
    }
}

impl Trace {
    fn new(tool: Tool) -> Self {
        Self {
            tool,
            session_id: None,
            cwd: None,
            repo: None,
            label: None,
            last_assistant_message: None,
            stop_hook_active: None,
            background_tasks: 0,
            background_clank_wait: false,
            effective_auto: None,
            role: None,
            disposition: None,
        }
    }

    fn record_input(&mut self, input: &HookInput) {
        self.session_id = Some(input.session_id.as_str().to_string());
        self.cwd = Some(input.cwd.clone());
        self.last_assistant_message = input
            .last_assistant_message
            .as_deref()
            .map(|m| truncate_chars(m, TRACE_TEXT_CAP));
        self.stop_hook_active = Some(input.stop_hook_active);
        self.background_tasks = input.background_tasks.len();
        self.background_clank_wait = input.has_background_clank_wait();
    }

    fn set_repo(&mut self, repo: &Path) {
        self.repo = Some(repo.to_path_buf());
    }

    /// Land the phase-1 `in_flight` record. Called ONLY once identity is
    /// resolved (the record's path is settled), right before the hook's
    /// slow, crash-prone work — the spawned peek/wait. So a leftover
    /// `in_flight` always means "died mid-decision" at the agent path,
    /// and the repo-level fallback only ever holds FINAL records (an
    /// unidentified fire's diagnostic), never a stranded in-flight from
    /// an identified run (codex 4e22861).
    fn mark_in_flight(&mut self) {
        self.write("in_flight", "");
    }

    fn set_label(&mut self, label: &AgentLabel) {
        self.label = Some(label.as_str().to_string());
    }

    fn set_disposition(&mut self, d: BgDisposition) {
        self.disposition = Some(match d {
            BgDisposition::YieldArmed => "yield_armed",
            BgDisposition::NeedsWorkCheck => "needs_work_check",
            BgDisposition::NoBackgroundWork => "no_background_work",
        });
    }

    fn finalize(&mut self, outcome: &HookOutcome) {
        let (decision, reason) = match outcome {
            HookOutcome::Continue { reason } => {
                ("continue", truncate_chars(reason, TRACE_TEXT_CAP))
            }
            HookOutcome::Silent { why } => (
                "silent",
                serde_json::to_value(why)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
            ),
            HookOutcome::Diagnostic { message } => {
                ("diagnostic", truncate_chars(message, TRACE_TEXT_CAP))
            }
        };
        self.write(decision, &reason);
    }

    fn write(&mut self, decision: &str, reason: &str) {
        // Nowhere sensible to write without a repo (cwd wasn't one).
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let path = match self.label.as_deref() {
            Some(label) => repo.join(format!(".clank/agents/{label}/stop-hook.json")),
            None => repo.join(".clank/stop-hook.json"),
        };
        let ts = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let record = serde_json::json!({
            "ts": ts,
            "tool": self.tool.as_str(),
            "session_id": self.session_id,
            "cwd": self.cwd,
            "repo": self.repo.as_ref().map(|r| r.display().to_string()),
            "label": self.label,
            "last_assistant_message": self.last_assistant_message,
            "stop_hook_active": self.stop_hook_active,
            "background_tasks": self.background_tasks,
            "background_clank_wait": self.background_clank_wait,
            "effective_auto": self.effective_auto.map(|m| format!("{m:?}").to_lowercase()),
            "role": self.role.map(|r| format!("{r:?}").to_lowercase()),
            "disposition": self.disposition,
            "decision": decision,
            "reason": reason,
        });
        let Ok(body) = serde_json::to_string_pretty(&record) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, body);
    }
}

async fn compute_outcome(
    tool: Tool,
    repo_override: Option<&Path>,
    input: HookInput,
    trace: &mut Trace,
) -> HookOutcome {
    // Decide how the agent's in-flight background work affects this
    // turn-end, BEFORE any clank resolution (`background_disposition` is a
    // pure function of the turn). `YieldArmed` never runs a wait — Claude
    // Code auto-wakes when the work completes, and running our own wait here
    // would block that wake. Only `NeedsWorkCheck` / `NoBackgroundWork` fall
    // through to resolution, so a config Diagnostic can only surface when we
    // were going to engage clank anyway.
    let disposition = background_disposition(tool, &input);
    trace.set_disposition(disposition);
    if matches!(disposition, BgDisposition::YieldArmed) {
        // Best-effort repo+label resolution PURELY for the trace — this
        // early-return deliberately precedes clank resolution, and a
        // failure here must stay a silent yield, never a Diagnostic.
        if let Ok(repo) = resolve_hook_repo(repo_override, &input) {
            if let Ok(label) = resolve_identity_for_hook(&repo, tool, &input.session_id) {
                trace.set_label(&label);
            }
            trace.set_repo(&repo);
        }
        return HookOutcome::Silent {
            why: SilentReason::YieldArmed,
        };
    }

    // Resolve repo: explicit --repo (testing) > hook stdin cwd.
    let repo = match resolve_hook_repo(repo_override, &input) {
        Ok(r) => r,
        Err(e) => return HookOutcome::Diagnostic { message: e },
    };
    trace.set_repo(&repo);

    let label = match resolve_identity_for_hook(&repo, tool, &input.session_id) {
        Ok(l) => l,
        Err(e) => {
            return HookOutcome::Diagnostic {
                message: format!("{e:#}"),
            };
        }
    };
    trace.set_label(&label);

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
    trace.effective_auto = Some(effective);

    // Role is roster-derived. The hook path is fail-soft: if the
    // repo has no master configured (or resolution errors),
    // default to Reviewer —
    // a misconfigured repo shouldn't block the agent's session,
    // and Reviewer is the conservative default (won't spuriously
    // drive master-only actions).
    let role = crate::agent_store::resolve_role(&repo, &label)
        .unwrap_or_else(|_| clank_core::vocab::Role::default());
    trace.role = Some(role);
    // Identity settled — land the phase-1 record before the slow work.
    trace.mark_in_flight();

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
            _ => {
                let wait_timeout = cfg.as_ref().and_then(|c| c.wait_timeout.clone());
                compute_wait_outcome(&repo, &label, role, wait_timeout.as_deref()).await
            }
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

/// The continuation that nudges the agent to start its own backgrounded
/// `clank wait` alongside a live process. Wording matches the spike that
/// validated compliance — explicit and copy-pasteable (a vague hint is the
/// failure mode). Deliberately does NOT echo the task command lines: the
/// agent knows what it backgrounded, and real commands (multi-clause
/// `until …; do sleep …` one-liners) turned the nudge into a wall of shell
/// that buried the instruction (lloyd, dark-skippy). Only the COUNT is
/// stated.
fn nudge_reason(input: &HookInput) -> String {
    let count = input
        .background_tasks
        .iter()
        .filter(|t| !t.is_clank_wait())
        .count();
    let what = if count > 1 {
        format!("{count} background tasks")
    } else {
        "a background task".to_string()
    };
    format!(
        "You ended your turn with {what} still running, but nothing is \
         watching for clank review work. Start `clank wait` as its OWN \
         background task now — call the Bash tool with command `clank wait` \
         and run_in_background set to true — then end your turn. That way \
         EITHER the background task finishing OR new clank review work will \
         wake you."
    )
}

/// Long-poll via a self-spawned `clank wait --json`.
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
    let timeout_arg = wait_timeout.unwrap_or("0");

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
/// minimal wake hint (`wfw-output-is-a-minimal-hint`). Anything
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
}

fn parse_wait_json(raw: &[u8]) -> Result<Vec<WaitItem>, String> {
    let envelope: WaitEnvelope =
        serde_json::from_slice(raw).map_err(|e| format!("not valid JSON: {e}"))?;
    Ok(envelope.items)
}

/// Render wait's JSON `items` array into the continuation prompt
/// body. Loose stringly-typed projection because we're consuming
/// our own JSON output via subprocess. One MINIMAL line per item
/// — who/verb + plan + 12-char sha (`wfw-output-is-a-minimal-hint`):
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
        // Minimal hints (`wfw-output-is-a-minimal-hint`): one line
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

    fn read_trace(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn auto_off_writes_a_silent_trace_with_the_branch_reason() {
        let dir = init_repo();
        let repo = dir.path();
        let label = bind(repo, "codex", "sess-auto-off");
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::Off).unwrap();

        let mut trace = Trace::new(Tool::Claude);
        let input = hook_input("sess-auto-off", Some("I finished the thing."));
        trace.record_input(&input);
        let outcome = compute_outcome(Tool::Claude, Some(repo), input, &mut trace).await;
        assert_eq!(
            outcome,
            HookOutcome::Silent {
                why: SilentReason::AutoOff
            }
        );
        trace.finalize(&outcome);

        let rec = read_trace(&repo.join(".clank/agents/codex/stop-hook.json"));
        assert_eq!(rec["decision"], "silent");
        assert_eq!(rec["reason"], "auto_off");
        assert_eq!(rec["label"], "codex");
        assert_eq!(rec["last_assistant_message"], "I finished the thing.");
        assert_eq!(rec["effective_auto"], "off");
    }

    #[tokio::test]
    async fn yield_armed_traces_without_engaging_clank_resolution() {
        // An armed background `clank wait` yields silently — but the trace
        // still records the fire, the branch, and (best-effort) the agent.
        let dir = init_repo();
        let repo = dir.path();
        bind(repo, "codex", "sess-armed");

        let input: HookInput = serde_json::from_value(serde_json::json!({
            "session_id": "sess-armed",
            "cwd": "/tmp",
            "stop_hook_active": false,
            "background_tasks": [
                {"id": "t1", "status": "running", "command": "clank wait --author codex"}
            ],
        }))
        .unwrap();
        let mut trace = Trace::new(Tool::Claude);
        trace.record_input(&input);
        let outcome = compute_outcome(Tool::Claude, Some(repo), input, &mut trace).await;
        assert_eq!(
            outcome,
            HookOutcome::Silent {
                why: SilentReason::YieldArmed
            }
        );
        trace.finalize(&outcome);

        let rec = read_trace(&repo.join(".clank/agents/codex/stop-hook.json"));
        assert_eq!(rec["reason"], "yield_armed");
        assert_eq!(rec["disposition"], "yield_armed");
        assert_eq!(rec["background_clank_wait"], true);
        assert_eq!(rec["background_tasks"], 1);
    }

    #[tokio::test]
    async fn unresolved_identity_falls_back_to_the_repo_level_trace() {
        // A session with NO binding: the hook fired but can't tell who it
        // is — the record lands at the repo level so this case is still
        // distinguishable from "the hook never fired".
        let dir = init_repo();
        let repo = dir.path();

        let mut trace = Trace::new(Tool::Claude);
        let input = hook_input("sess-unbound", None);
        trace.record_input(&input);
        let outcome = compute_outcome(Tool::Claude, Some(repo), input, &mut trace).await;
        assert!(matches!(outcome, HookOutcome::Diagnostic { .. }));
        trace.finalize(&outcome);

        let rec = read_trace(&repo.join(".clank/stop-hook.json"));
        assert_eq!(rec["decision"], "diagnostic");
        assert_eq!(rec["label"], serde_json::Value::Null);
        assert_eq!(rec["last_assistant_message"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn identified_run_leaves_no_repo_level_record() {
        // codex 4e22861: an identified run must never strand anything at
        // the repo-level fallback — in_flight lands only at the AGENT path
        // (once identity settles), so a leftover in_flight always means
        // died-mid-decision. A previous unidentified fire's FINAL record
        // at the fallback survives an identified run untouched.
        let dir = init_repo();
        let repo = dir.path();
        std::fs::create_dir_all(repo.join(".clank")).unwrap();
        std::fs::write(
            repo.join(".clank/stop-hook.json"),
            r#"{"decision":"diagnostic","reason":"earlier unbound fire"}"#,
        )
        .unwrap();
        let label = bind(repo, "codex", "sess-identified");
        crate::agent_store::set_auto_mode(repo, &label, AutoMode::Off).unwrap();

        let mut trace = Trace::new(Tool::Claude);
        let input = hook_input("sess-identified", None);
        trace.record_input(&input);
        let outcome = compute_outcome(Tool::Claude, Some(repo), input, &mut trace).await;
        trace.finalize(&outcome);

        let rec = read_trace(&repo.join(".clank/agents/codex/stop-hook.json"));
        assert_eq!(rec["decision"], "silent", "final, not in_flight");
        let fallback = read_trace(&repo.join(".clank/stop-hook.json"));
        assert_eq!(
            fallback["reason"], "earlier unbound fire",
            "the previous unidentified record survives an identified run"
        );
    }

    #[test]
    fn trace_write_failure_is_swallowed_and_no_repo_writes_nothing() {
        // No repo resolved → nowhere to write; must not panic.
        let mut t = Trace::new(Tool::Claude);
        t.finalize(&HookOutcome::Diagnostic {
            message: "no repo".into(),
        });
        // Repo whose .clank is a FILE → create_dir_all/write fail; still
        // silent (best-effort IO can never change the hook's behavior).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".clank"), "not a dir").unwrap();
        let mut t = Trace::new(Tool::Claude);
        t.repo = Some(dir.path().to_path_buf());
        t.finalize(&HookOutcome::Silent {
            why: SilentReason::NoWork,
        });
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
                {"id": "t3", "status": "running", "command": "clank wait --author codex"},
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
        // The spike-validated instruction is intact.
        assert!(reason.contains("Start `clank wait` as its OWN"));
        assert!(reason.contains("run_in_background set to true"));

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

    #[test]
    fn trace_text_is_truncated_at_the_cap() {
        let long = "x".repeat(TRACE_TEXT_CAP + 500);
        let t = truncate_chars(&long, TRACE_TEXT_CAP);
        assert_eq!(t.chars().count(), TRACE_TEXT_CAP + 1, "cap + ellipsis");
        assert!(t.ends_with('…'));
        assert_eq!(truncate_chars("short", TRACE_TEXT_CAP), "short");
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
        // (wfw-output-is-a-minimal-hint). One wake with every
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
