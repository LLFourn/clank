//! Typed shapes for the Stop-hook stdin/stdout protocol.
//!
//! Both claude and codex pass JSON on the hook's stdin and read
//! the hook's exit code + stdout/stderr to decide whether to
//! continue the agent. The wire shapes look like:
//!
//! - claude stdin: `{ session_id, transcript_path, cwd,
//!   stop_hook_active, last_assistant_message, ... }` plus
//!   claude-only extras (`effort`, `background_tasks`, ...).
//! - codex stdin: same common fields plus codex-only extras
//!   (`turn_id`, `model`, ...).
//!
//! [`HookInput`] captures just the common fields the stop-hook
//! adapter actually uses; unknown fields are silently ignored
//! (`#[serde(default)]` + a deny-list of zero extras).
//!
//! [`HookOutcome`] is what the stop-hook adapter decides; the
//! per-tool wire form is the CLI's job to render — see the
//! `claude_*` / `codex_*` helpers and the [`CodexBlockDecision`]
//! struct (which serializes to codex's `{"decision":"block",
//! "reason":"..."}` stdout shape).
//!
//! Per-tool exit-code contract (from the plan):
//!
//! | Outcome             | Claude                  | Codex                                          |
//! | ---                 | ---                     | ---                                            |
//! | `Continue { reason }` | exit 2, stderr=reason | exit 0, stdout=`{decision:"block",reason:...}` |
//! | `Silent`            | exit 0, no output       | exit 0, no output                              |
//! | `Diagnostic { msg }` | exit 0, stderr=msg     | exit 0, stderr=msg                             |

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;
use crate::vocab::Tool;

/// Common fields the stop-hook adapter reads from hook stdin.
/// Both tools include all of these.
///
/// `#[serde(default)]` on optional fields and ignoring unknown
/// extras (claude's `effort` / `background_tasks` / `session_crons`,
/// codex's `turn_id` / `model`) keeps the type forward-compatible
/// with either agent adding fields later.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HookInput {
    pub session_id: SessionId,
    pub cwd: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// True iff this hook invocation is itself a continuation of a
    /// previous Stop-hook decision. The adapter does not act on
    /// this — claude's own 8-block cap (which resets on tool use
    /// between fires) is the safety net for runaway loops.
    pub stop_hook_active: bool,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    /// Claude's in-flight background work at turn end:
    /// `run_in_background` shells, subagents, MCP monitors, … . Claude
    /// lists ONLY live tasks here (`[]` once they finish) and re-fires
    /// Stop when one completes — that's the documented signal for "the
    /// turn ended because the session is *paused waiting for background
    /// work to wake it back up*," not because it's idle. See
    /// [`background_gate`]. Codex omits the field, so it stays empty there.
    #[serde(default)]
    pub background_tasks: Vec<BackgroundTask>,
}

/// One entry of claude's Stop-hook `background_tasks` array. `command`
/// (the raw shell command line for `shell` tasks) is captured so the hook
/// can recognize the agent's own backgrounded `clank wait`; the rest
/// (`description`, `type`, …) are ignored. Forward-compatible: every field
/// is optional.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BackgroundTask {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
}

impl BackgroundTask {
    /// True if this task is the agent's own `clank wait` long-poll.
    /// Best-effort: the program word's basename is `clank` and the first
    /// arg is `wait`. Claude reports the raw command the agent launched, so
    /// wrappers (`bash -lc '…'`, env prefixes) aren't unpicked — they're not
    /// how agents start it.
    pub fn is_clank_wait(&self) -> bool {
        let Some(cmd) = self.command.as_deref() else {
            return false;
        };
        let mut words = cmd.split_whitespace();
        let prog = words
            .next()
            .map(|w| w.rsplit(['/', '\\']).next().unwrap_or(w));
        prog == Some("clank") && words.next() == Some("wait")
    }
}

impl HookInput {
    /// True if a backgrounded `clank wait` is already watching for review
    /// work — the hook should yield to it rather than run its own wait.
    pub fn has_background_clank_wait(&self) -> bool {
        self.background_tasks
            .iter()
            .any(BackgroundTask::is_clank_wait)
    }

    /// True if any in-flight background task is NOT a `clank wait` — a
    /// process the agent is parked on. Claude Code wakes the session when it
    /// completes, so the hook must not block in a wait while it runs.
    pub fn has_non_wait_background_work(&self) -> bool {
        self.background_tasks.iter().any(|t| !t.is_clank_wait())
    }
}

/// The Stop hook's decision about the turn's background work — a pure
/// function of the turn (no clank identity/auto-mode), so the [`BgGate::Yield`]
/// cases can be decided before any repo/identity resolution. See
/// [`background_gate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgGate {
    /// Yield (Silent), no wait: a backgrounded `clank wait` already covers
    /// review-work wake, OR a process is live but we've already nudged /
    /// this isn't claude. Either way, running our own wait would block the
    /// wake Claude Code is about to deliver.
    Yield,
    /// A process is live with no `clank wait` watching: if the agent is
    /// being driven (auto on), nudge it to start `clank wait` alongside so
    /// either wake source is armed.
    NudgeIfDriving,
    /// No background work — proceed to the normal idle wait.
    NoBackgroundWork,
}

/// Pure background-work gate for the Stop hook. Claude's `background_tasks`
/// drives it; codex omits the field, so this is always
/// [`BgGate::NoBackgroundWork`] there. `tool` and `stop_hook_active` gate the
/// nudge: only claude auto-wakes on its own `run_in_background` tasks, and an
/// already-active stop chain shouldn't re-nudge.
pub fn background_gate(tool: Tool, input: &HookInput) -> BgGate {
    if input.has_background_clank_wait() {
        return BgGate::Yield;
    }
    if input.has_non_wait_background_work() {
        if tool != Tool::Claude || input.stop_hook_active {
            return BgGate::Yield;
        }
        return BgGate::NudgeIfDriving;
    }
    BgGate::NoBackgroundWork
}

/// What the stop-hook adapter decides to emit after evaluating
/// agent config + work state. CLI maps to per-tool wire output
/// (see [`claude_continuation_exit_code`],
/// [`codex_continuation_stdout`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Resume the agent with `reason` as a continuation prompt.
    /// The agent runs another turn against this text.
    Continue { reason: String },
    /// Let the agent stop normally. No output to either tool.
    /// Used for: `auto_mode=off`, wait timed out with no work.
    Silent,
    /// Internal problem (config parse, identity unresolvable,
    /// projection failure). The hook NEVER fails the agent —
    /// the diagnostic goes to stderr and exit is still 0 so the
    /// turn ends cleanly. Clank bugs show up here; production
    /// failures don't compound by also breaking the agent.
    Diagnostic { message: String },
}

/// Codex's stop-hook continuation wire shape, written to stdout
/// when the adapter wants to resume codex with a prompt.
///
/// Codex reads `decision="block"` as "do not stop, continue with
/// `reason` as the next user prompt." (`decision` is always
/// `"block"` for our use; codex supports other values that don't
/// apply here.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexBlockDecision<'a> {
    pub decision: &'static str,
    pub reason: &'a str,
}

impl<'a> CodexBlockDecision<'a> {
    pub fn from_reason(reason: &'a str) -> Self {
        Self {
            decision: "block",
            reason,
        }
    }
}

/// Exit code claude wants for a continuation. (`2` per
/// claude's hook protocol — the agent reads stderr as the
/// continuation prompt.)
pub const CLAUDE_CONTINUATION_EXIT: i32 = 2;

/// Exit code every other outcome maps to, regardless of tool.
pub const HOOK_OK_EXIT: i32 = 0;

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }

    #[test]
    fn hook_input_deserializes_minimal_claude_payload() {
        let raw = r#"{
            "session_id": "742f6a04-f174-409a-ab01-419a16c5f372",
            "cwd": "/repo",
            "stop_hook_active": false
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed.session_id,
            sid("742f6a04-f174-409a-ab01-419a16c5f372")
        );
        assert_eq!(parsed.cwd, "/repo");
        assert!(!parsed.stop_hook_active);
        assert!(parsed.transcript_path.is_none());
        assert!(parsed.last_assistant_message.is_none());
    }

    #[test]
    fn hook_input_deserializes_full_claude_payload_ignoring_extras() {
        // Real-world claude hook stdin we captured during the
        // E1 experiment. Includes extras (`effort`, `permission_mode`,
        // `background_tasks`, `session_crons`) that core ignores.
        let raw = r#"{
            "session_id": "bcb7104f-7b4c-4715-8c3d-f1c020760d06",
            "transcript_path": "/Users/llfourn/.claude/projects/-x/y.jsonl",
            "cwd": "/private/tmp/clank-hook-experiment/claude",
            "permission_mode": "auto",
            "effort": {"level": "xhigh"},
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": "Hello! How can I help you today?",
            "background_tasks": [],
            "session_crons": []
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed.session_id,
            sid("bcb7104f-7b4c-4715-8c3d-f1c020760d06")
        );
        assert_eq!(
            parsed.transcript_path.as_deref(),
            Some("/Users/llfourn/.claude/projects/-x/y.jsonl")
        );
        assert_eq!(
            parsed.last_assistant_message.as_deref(),
            Some("Hello! How can I help you today?")
        );
        // Empty `background_tasks` == genuinely idle turn.
        assert!(parsed.background_tasks.is_empty());
        assert!(!parsed.has_non_wait_background_work());
        assert!(!parsed.has_background_clank_wait());
    }

    #[test]
    fn live_background_task_means_paused_not_idle() {
        // Captured claude Stop stdin while a `run_in_background` shell
        // was still running. A non-empty `background_tasks` is claude's
        // "paused waiting for background work to wake me back up" signal:
        // the stop hook must yield, not wait.
        let raw = r#"{
            "session_id": "bcb7104f-7b4c-4715-8c3d-f1c020760d06",
            "cwd": "/repo",
            "stop_hook_active": false,
            "background_tasks": [
                {"id": "bz32mj8uz", "type": "shell", "status": "running",
                 "description": "Sleep for 8 seconds in background", "command": "sleep 8"}
            ],
            "session_crons": []
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.background_tasks.len(), 1);
        assert_eq!(parsed.background_tasks[0].id.as_deref(), Some("bz32mj8uz"));
        assert_eq!(
            parsed.background_tasks[0].status.as_deref(),
            Some("running")
        );
        // A non-wait process: live work, but no `clank wait` among it.
        assert!(parsed.has_non_wait_background_work());
        assert!(!parsed.has_background_clank_wait());
    }

    #[test]
    fn missing_background_tasks_field_is_idle() {
        // Codex omits the field entirely; absence must read as idle, not
        // paused (so codex sessions always reach the wait).
        let raw = r#"{
            "session_id": "019e5385-ed97-7603-8561-dd9024328ff9",
            "cwd": "/repo",
            "stop_hook_active": false
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert!(parsed.background_tasks.is_empty());
        assert!(!parsed.has_non_wait_background_work());
        assert!(!parsed.has_background_clank_wait());
    }

    fn task(command: &str) -> BackgroundTask {
        BackgroundTask {
            id: None,
            status: Some("running".into()),
            command: Some(command.into()),
        }
    }

    fn input_with(tasks: Vec<BackgroundTask>, stop_hook_active: bool) -> HookInput {
        HookInput {
            session_id: sid("bcb7104f-7b4c-4715-8c3d-f1c020760d06"),
            cwd: "/repo".into(),
            transcript_path: None,
            stop_hook_active,
            last_assistant_message: None,
            background_tasks: tasks,
        }
    }

    #[test]
    fn is_clank_wait_matches_real_command_shapes() {
        for cmd in [
            "clank wait",
            "clank wait --repo /x --author claude",
            "/Users/x/.cargo/bin/clank wait",
            "./clank wait",
        ] {
            assert!(task(cmd).is_clank_wait(), "should match: {cmd}");
        }
        for cmd in [
            "clank status",
            "clank waitx",
            "clankwait",
            "sleep 12",
            "git wait",
            "clank",
        ] {
            assert!(!task(cmd).is_clank_wait(), "should NOT match: {cmd}");
        }
        // No command (non-shell task) is never a clank wait.
        assert!(
            !BackgroundTask {
                id: Some("x".into()),
                status: Some("running".into()),
                command: None,
            }
            .is_clank_wait()
        );
    }

    #[test]
    fn background_gate_covers_the_state_machine() {
        use Tool::{Claude, Codex};
        // A backgrounded `clank wait` (with or without other work) → Yield.
        assert_eq!(
            background_gate(Claude, &input_with(vec![task("clank wait")], false)),
            BgGate::Yield
        );
        assert_eq!(
            background_gate(
                Claude,
                &input_with(vec![task("sleep 60"), task("clank wait")], false)
            ),
            BgGate::Yield
        );
        // A live process, no clank wait, claude, first stop → nudge.
        assert_eq!(
            background_gate(Claude, &input_with(vec![task("sleep 60")], false)),
            BgGate::NudgeIfDriving
        );
        // Already nudged (stop_hook_active) → Yield (don't nag).
        assert_eq!(
            background_gate(Claude, &input_with(vec![task("sleep 60")], true)),
            BgGate::Yield
        );
        // Codex has no run_in_background auto-wake → never nudge.
        assert_eq!(
            background_gate(Codex, &input_with(vec![task("sleep 60")], false)),
            BgGate::Yield
        );
        // No background work → proceed to the normal wait.
        assert_eq!(
            background_gate(Claude, &input_with(vec![], false)),
            BgGate::NoBackgroundWork
        );
    }

    #[test]
    fn hook_input_deserializes_full_codex_payload_ignoring_extras() {
        // Captured codex stdin. Extras: `turn_id`, `model`,
        // `permission_mode`, `hook_event_name`.
        let raw = r#"{
            "session_id": "019e5385-ed97-7603-8561-dd9024328ff9",
            "turn_id": "019e5386-1684-7250-a7fc-1823f4b8c1be",
            "transcript_path": "/Users/llfourn/.codex/sessions/rollout.jsonl",
            "cwd": "/private/tmp/clank-hook-experiment/codex",
            "hook_event_name": "Stop",
            "model": "gpt-5.5",
            "permission_mode": "default",
            "stop_hook_active": false,
            "last_assistant_message": "Hello. How can I help?"
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed.session_id,
            sid("019e5385-ed97-7603-8561-dd9024328ff9")
        );
        assert!(!parsed.stop_hook_active);
    }

    #[test]
    fn hook_input_rejects_missing_required_field() {
        let raw = r#"{"cwd":"/x","stop_hook_active":false}"#;
        let result: Result<HookInput, _> = serde_json::from_str(raw);
        assert!(result.is_err(), "expected missing session_id to fail");
    }

    #[test]
    fn hook_input_rejects_invalid_session_id() {
        let raw = r#"{
            "session_id": "not/a/valid/id",
            "cwd": "/x",
            "stop_hook_active": false
        }"#;
        let result: Result<HookInput, _> = serde_json::from_str(raw);
        assert!(result.is_err(), "expected invalid session id to fail");
    }

    #[test]
    fn codex_block_decision_serializes_correctly() {
        let decision = CodexBlockDecision::from_reason("run wait now");
        let json = serde_json::to_string(&decision).unwrap();
        assert_eq!(json, r#"{"decision":"block","reason":"run wait now"}"#);
    }

    #[test]
    fn codex_block_decision_escapes_reason_with_specials() {
        let decision = CodexBlockDecision::from_reason("line 1\n\"quoted\"\tend");
        let json = serde_json::to_string(&decision).unwrap();
        // serde_json handles all the JSON escaping for us.
        assert_eq!(
            json,
            r#"{"decision":"block","reason":"line 1\n\"quoted\"\tend"}"#
        );
    }

    #[test]
    fn hook_outcome_variants_are_distinct() {
        // Sanity that we can match exhaustively on the public enum.
        let outcomes = [
            HookOutcome::Continue { reason: "x".into() },
            HookOutcome::Silent,
            HookOutcome::Diagnostic {
                message: "y".into(),
            },
        ];
        for o in &outcomes {
            match o {
                HookOutcome::Continue { .. } => {}
                HookOutcome::Silent => {}
                HookOutcome::Diagnostic { .. } => {}
            }
        }
    }

    #[test]
    fn continuation_exit_codes_are_correct() {
        assert_eq!(CLAUDE_CONTINUATION_EXIT, 2);
        assert_eq!(HOOK_OK_EXIT, 0);
    }
}
