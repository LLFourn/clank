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
    AutoMode, CLAUDE_CONTINUATION_EXIT, CodexBlockDecision, HOOK_OK_EXIT, HookInput, HookOutcome,
    Role, Tool,
};

pub async fn run(args: StopHookArgs) -> anyhow::Result<()> {
    let tool: Tool = args.tool.into();
    let outcome = compute_outcome(tool, args.repo.as_deref()).await;
    emit_and_exit(outcome, tool);
}

async fn compute_outcome(tool: Tool, repo_override: Option<&Path>) -> HookOutcome {
    let input = match read_hook_stdin() {
        Ok(i) => i,
        Err(e) => return HookOutcome::Diagnostic { message: e },
    };

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

    let cfg = match load_agent_config(&repo, &label) {
        Ok(Some(c)) => c,
        // No config yet (agent was bound via `clank as` but never
        // ran `clank auto on`) — treat as Off, exit silently.
        Ok(None) => return HookOutcome::Silent,
        Err(e) => {
            return HookOutcome::Diagnostic {
                message: format!("{e:#}"),
            };
        }
    };

    // Role is team-derived (`teams-based-agent-registration`).
    // The hook path is fail-soft: if the repo has no team
    // configured (or resolution errors), default to Reviewer —
    // a misconfigured repo shouldn't block the agent's session,
    // and Reviewer is the conservative default (won't spuriously
    // drive master-only actions).
    let role = crate::agent_store::resolve_role(&repo, &label)
        .unwrap_or_else(|_| clank_core::vocab::Role::default());

    match cfg.auto_mode {
        AutoMode::Off => HookOutcome::Silent,
        AutoMode::On => compute_wait_outcome(&repo, &label, role, cfg.wfw_timeout.as_deref()).await,
    }
}

/// Long-poll via a self-spawned `clank wfw --json`.
/// Reuses the watcher loop without refactoring it. On items →
/// Continue. On timeout (exit 2) → Silent. Anything else →
/// Diagnostic.
///
/// Self-spawning has one advantage over factoring out the loop:
/// the wfw process is a clean child that gets killed if the
/// agent kills the hook (Stdio::piped + drop kills the child).
async fn compute_wait_outcome(
    repo: &Path,
    label: &AgentLabel,
    role: Role,
    wfw_timeout: Option<&str>,
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
    let timeout_arg = wfw_timeout.unwrap_or("0");

    let mut cmd = Command::new(&exe);
    cmd.arg("wfw")
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
                message: format!("hook: spawning wfw failed: {e}"),
            };
        }
    };

    match output.status.code() {
        Some(0) => match parse_wfw_json(&output.stdout) {
            Ok(items) if items.is_empty() => HookOutcome::Silent,
            Ok(items) => HookOutcome::Continue {
                reason: render_wfw_items(&items, label, role),
            },
            Err(e) => HookOutcome::Diagnostic {
                message: format!("hook: wfw stdout malformed: {e}"),
            },
        },
        Some(2) => HookOutcome::Silent, // wfw timeout
        other => HookOutcome::Diagnostic {
            message: format!(
                "hook: wfw exited {} stderr={}",
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

fn parse_wfw_json(raw: &[u8]) -> Result<Vec<serde_json::Value>, String> {
    let envelope: serde_json::Value =
        serde_json::from_slice(raw).map_err(|e| format!("not valid JSON: {e}"))?;
    let items = envelope
        .get("items")
        .ok_or_else(|| "missing `items` field".to_string())?;
    items
        .as_array()
        .cloned()
        .ok_or_else(|| "`items` is not an array".to_string())
}

/// Render wfw's JSON `items` array into the continuation prompt
/// body. Loose stringly-typed projection because we're consuming
/// our own JSON output via subprocess. One MINIMAL line per item
/// — who/verb + plan + 12-char sha (`wfw-output-is-a-minimal-hint`):
/// the HOW (feedback-write form, verdicts, promote evaluation,
/// unblock) lives in the agent's skill doc, not re-taught per
/// wake.
fn render_wfw_items(items: &[serde_json::Value], label: &AgentLabel, role: Role) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Clank wfw returned work for `{label}` ({role}). Items:\n",
        label = label.as_str(),
        role = role.as_str(),
    ));
    for item in items {
        let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let plan = item.get("plan").and_then(|v| v.as_str()).unwrap_or("?");
        let full = item
            .get("sha")
            .or_else(|| item.get("finalized_at"))
            .and_then(|v| v.as_str())
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
                let next = item.get("next").and_then(|v| v.as_str()).unwrap_or("?");
                let reason = item.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
                out.push_str(&format!("  - master: {next} {plan} @ {short} ({reason})\n"));
            }
            "finished" => out.push_str(&format!("  - finished: {plan} @ {short}\n")),
            "idle" => {
                let prompt = item.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(&format!("  - idle: {prompt}\n"));
            }
            "adhoc_review" => out.push_str(&format!("  - adhoc-review: {short}\n")),
            "adhoc_revise" => out.push_str(&format!("  - adhoc-revise: {short}\n")),
            "promote_from_queue" => {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let priority = item.get("priority").and_then(|v| v.as_u64()).unwrap_or(0);
                out.push_str(&format!("  - promote: {name} (priority {priority:03})\n"));
            }
            "blocked" => {
                let agent = item.get("agent").and_then(|v| v.as_str()).unwrap_or("?");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let scope = item.get("plan").and_then(|v| v.as_str()).unwrap_or("repo");
                out.push_str(&format!(
                    "  - blocked: {agent}/{name} on {scope} (awaiting human)\n"
                ));
            }
            "unblocked" => {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                // The answer is the action payload (the human's
                // instruction) — signal, not tutorial.
                let answer = item.get("answer").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(&format!("  - unblocked: {name}: {answer}\n"));
            }
            "pr_reviewer" => {
                let pr = item.get("pr").and_then(|v| v.as_u64()).unwrap_or(0);
                let round = item.get("round").and_then(|v| v.as_u64()).unwrap_or(0);
                out.push_str(&format!("  - pr-review: review #{pr} round {round}\n"));
            }
            "pr_master" => {
                let pr = item.get("pr").and_then(|v| v.as_u64()).unwrap_or(0);
                let round = item.get("round").and_then(|v| v.as_u64()).unwrap_or(0);
                let next = item.get("next").and_then(|v| v.as_str()).unwrap_or("?");
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

/// 12 chars, matching wfw's hint width: agents compose
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
        (HookOutcome::Silent, _) => {
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

    fn items_text(items: &[serde_json::Value]) -> String {
        render_wfw_items(items, &AgentLabel::parse("codex").unwrap(), Role::Reviewer)
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
            "approve|finished|request-changes",
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
}
