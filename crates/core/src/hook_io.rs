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

/// Common fields the stop-hook adapter reads from hook stdin.
/// Both tools include all of these.
///
/// `#[serde(default)]` on optional fields and ignoring unknown
/// extras (claude's `effort` / `session_crons`, codex's `model`)
/// keeps the type forward-compatible with either agent adding fields
/// later.
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
    /// The harness's own live background work at this Stop. `None` when
    /// the payload omits the field (codex), which says nothing about
    /// whether anything is running; `Some(vec![])` says nothing is.
    #[serde(default)]
    pub background_tasks: Option<Vec<BackgroundTask>>,
}

/// One entry of a harness's Stop-time task listing: Claude's
/// `background_tasks`, Grok's `backgroundTasks`.
///
/// Observed shapes (`crates/core/tests/fixtures/stop_hook/`): a subagent is
/// `{id, type: "subagent", status, description, agent_type}` with its name
/// in `description`; a finished task leaves the list rather than turning
/// `completed`. Every field is optional, and `raw` keeps the whole entry as
/// observed, so a field clank does not read is never lost.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "serde_json::Value", into = "serde_json::Value")]
pub struct BackgroundTask {
    /// The harness's stable id for the task.
    pub id: Option<String>,
    pub status: Option<String>,
    /// `subagent`, `shell`, `monitor`, … .
    pub kind: Option<String>,
    /// A subagent's name; what a `clank attend` declaration matches.
    pub description: Option<String>,
    pub raw: serde_json::Value,
}

impl BackgroundTask {
    /// Still running by its own status: a harness that keeps a finished
    /// task in the list marks it `completed`.
    pub fn live(&self) -> bool {
        self.status.as_deref() != Some("completed")
    }
}

impl From<BackgroundTask> for serde_json::Value {
    fn from(task: BackgroundTask) -> Self {
        task.raw
    }
}

impl From<serde_json::Value> for BackgroundTask {
    fn from(raw: serde_json::Value) -> Self {
        let text = |key: &str| raw.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        Self {
            id: text("id"),
            status: text("status"),
            kind: text("type"),
            description: text("description"),
            raw,
        }
    }
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

    fn fixture(name: &str) -> HookInput {
        let path = format!(
            "{}/tests/fixtures/stop_hook/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    /// Claude's captured Stops: a running subagent is listed by its id and
    /// name, a finished one leaves an empty list, and a payload without the
    /// field (codex) is no listing at all rather than an empty one.
    #[test]
    fn a_claude_stop_carries_its_listing_and_an_omitted_one_is_none() {
        let running = fixture("stop_subagent_running.json");
        let tasks = running.background_tasks.expect("a listing");
        let [task] = tasks.as_slice() else {
            panic!("{tasks:?}")
        };
        assert_eq!(task.id.as_deref(), Some("a25eee2eef81caf3e"));
        assert_eq!(task.kind.as_deref(), Some("subagent"));
        assert_eq!(task.description.as_deref(), Some("capture-probe sleeper"));
        assert!(task.live());
        assert_eq!(task.raw["agent_type"], "general-purpose", "kept whole");

        assert_eq!(
            fixture("stop_subagent_finished.json").background_tasks,
            Some(vec![])
        );
        let bare: HookInput = serde_json::from_str(
            r#"{"session_id":"session-one","cwd":"/r","stop_hook_active":false}"#,
        )
        .unwrap();
        assert_eq!(bare.background_tasks, None);
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
