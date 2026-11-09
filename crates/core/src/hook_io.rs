//! Typed shapes for the Stop-hook stdin/stdout protocol.
//!
//! Both claude and codex pass JSON on the hook's stdin and read
//! the hook's exit code + stdout/stderr to decide whether to
//! continue the agent. The wire shapes look like:
//!
//! - claude stdin: `{ session_id, transcript_path, cwd,
//!   stop_hook_active, last_assistant_message, ... }` plus
//!   claude-only extras (`effort`, `background_tasks`, ...), none of
//!   which clank reads.
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
    /// between fires) is the safety net for runaway loops. Claude's
    /// PostCompact, which carries the same common fields, sends none.
    #[serde(default)]
    pub stop_hook_active: bool,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    /// Codex's id for the turn this Stop ends. Claude and OpenCode do not
    /// send one.
    #[serde(default)]
    pub turn_id: Option<String>,
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
    /// The OpenCode plugin must start another long-poll after this
    /// one's ceiling. This is a plugin control action, not a model
    /// prompt; it keeps the wait alive without an idle-turn loop.
    RenewWait,
    /// Let the agent stop normally. No output to either tool. `why`
    /// carries WHICH silent branch fired — never on the wire, but
    /// first-class so "the hook chose not to wait" stays
    /// distinguishable per-branch (and testable) instead of one
    /// indistinct silence.
    Silent { why: SilentReason },
    /// Internal problem (config parse, identity unresolvable,
    /// projection failure). The hook NEVER fails the agent —
    /// the diagnostic goes to stderr and exit is still 0 so the
    /// turn ends cleanly. Clank bugs show up here; production
    /// failures don't compound by also breaking the agent.
    Diagnostic { message: String },
}

/// One declaration per outcome: enum, wire token and `ALL` together,
/// for the same reason `silent_reasons!` does it — a second
/// handwritten list is what this plan keeps deleting.
/// One declaration per reason: the enum and [`SilentReason::ALL`] are
/// generated from it together.
///
/// A second handwritten list is exactly what milestone 1 deleted, and
/// adding one back here was not caught by a test that MATCHED on the
/// list rather than on the enum: a new variant plus its match arm,
/// with `ALL` left alone, passed (codex cfb98cbc83a9).
macro_rules! silent_reasons {
    ($($(#[$doc:meta])* $variant:ident),+ $(,)?) => {
        /// Why the hook let the agent stop silently — first-class data
        /// so each branch is named rather than re-derived at the call
        /// sites.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum SilentReason {
            $($(#[$doc])* $variant),+
        }

        impl SilentReason {
            /// Every reason. Generated, so it cannot omit one — which
            /// is what lets a caller quantify over the whole space
            /// instead of a hand-picked subset.
            pub const ALL: &'static [SilentReason] = &[$(SilentReason::$variant),+];
        }
    };
}

silent_reasons! {
    /// Effective auto-mode is off.
    AutoOff,
    /// This hook's session no longer holds the label.
    StaleBinding,
    /// The codex client that spawned this hook has exited; nobody is
    /// left to deliver to.
    OwnerGone,
    /// A native handoff's `clank wait` already carries the next wake.
    WaitArmed,
    /// A person's signed command waits for this agent's idle prompt.
    UserCommand,
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

    /// A harness's task listing is an extra like any other: a payload that
    /// carries one still parses, and nothing in what clank reads holds it.
    #[test]
    fn a_payload_with_a_task_listing_parses_and_keeps_none_of_it() {
        let bare = r#"{"session_id":"session-one","cwd":"/repo","stop_hook_active":false}"#;
        let listed = r#"{
            "session_id": "session-one",
            "cwd": "/repo",
            "stop_hook_active": false,
            "background_tasks": [
                {"id": "a25eee2", "type": "subagent", "status": "running",
                 "description": "capture-probe sleeper"},
                {"id": "b1", "type": "shell", "command": "cargo test"}
            ]
        }"#;
        let bare: HookInput = serde_json::from_str(bare).unwrap();
        let listed: HookInput = serde_json::from_str(listed).unwrap();
        assert_eq!(listed, bare);
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
            HookOutcome::RenewWait,
            HookOutcome::Silent {
                why: SilentReason::AutoOff,
            },
            HookOutcome::Diagnostic {
                message: "y".into(),
            },
        ];
        for o in &outcomes {
            match o {
                HookOutcome::Continue { .. } => {}
                HookOutcome::RenewWait => {}
                HookOutcome::Silent { .. } => {}
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
