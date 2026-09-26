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
    /// the attendance check. Codex omits the field, so it stays empty
    /// there.
    #[serde(default)]
    pub background_tasks: Vec<BackgroundTask>,
}

/// One entry of claude's Stop-hook `background_tasks` array. `command`
/// (the raw shell command line for `shell` tasks) is captured so the hook
/// can identify a background task; the rest
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

#[derive(Debug, PartialEq, Eq)]
enum ShellToken {
    Word(String),
    Boundary,
}

/// Index of the actual program word, skipping leading `VAR=value`
/// assignments and `env` / `exec` / `command` wrappers.
///
/// Shared by every command recogniser here so the wrapper rules cannot
/// drift apart between them.
fn command_word_index(words: &[&str]) -> usize {
    let mut command = 0;
    loop {
        while words.get(command).is_some_and(|word| is_assignment(word)) {
            command += 1;
        }
        match words.get(command).map(|word| program_basename(word)) {
            Some("env" | "exec" | "command") => {
                command += 1;
                if words.get(command) == Some(&"--") {
                    command += 1;
                }
            }
            _ => return command,
        }
    }
}

/// The `--desc` VALUE of a `clank run` invocation, or `None` if this
/// command line is not one.
///
/// Structured recognition, not substring matching: `echo "test run"`
/// names the same words and is not a run, and a `clank run` whose
/// description happens to appear later on its own line is not a match
/// for that later text either. The description is the only handle the
/// hook has on an owned run — the tool assigns a task id only after
/// launching the command, so no id can exist in the marker — which is
/// exactly why the handle has to be recognised precisely.
///
/// Stops at `--`: everything after it belongs to the command being
/// run, so a `--desc` there is the CHILD's flag, not clank's.
pub fn clank_run_desc(command: &str) -> Option<String> {
    let tokens = shell_tokens(command);
    tokens
        .split(|token| matches!(token, ShellToken::Boundary))
        .find_map(|segment| {
            let words: Vec<&str> = segment
                .iter()
                .filter_map(|token| match token {
                    ShellToken::Word(word) => Some(word.as_str()),
                    ShellToken::Boundary => None,
                })
                .collect();
            let at = command_word_index(&words);
            if words.get(at).map(|w| program_basename(w)) != Some("clank")
                || words.get(at + 1) != Some(&"run")
            {
                return None;
            }
            let mut i = at + 2;
            while let Some(word) = words.get(i) {
                if *word == "--" {
                    return None;
                }
                if *word == "--desc" {
                    return words.get(i + 1).map(|v| v.trim().to_string());
                }
                if let Some(v) = word.strip_prefix("--desc=") {
                    return Some(v.trim().to_string());
                }
                i += 1;
            }
            None
        })
        .filter(|d| !d.is_empty())
}

fn program_basename(word: &str) -> &str {
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some('_' | 'a'..='z' | 'A'..='Z'))
        && chars.all(|ch| matches!(ch, '_' | 'a'..='z' | 'A'..='Z' | '0'..='9'))
}

fn shell_tokens(command: &str) -> Vec<ShellToken> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                word_started = true;
                for quoted in chars.by_ref() {
                    if quoted == '\'' {
                        break;
                    }
                    word.push(quoted);
                }
            }
            '"' => {
                word_started = true;
                while let Some(quoted) = chars.next() {
                    match quoted {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        _ => word.push(quoted),
                    }
                }
            }
            '\\' => {
                word_started = true;
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            '#' if !word_started => {
                for comment in chars.by_ref() {
                    if comment == '\n' {
                        push_boundary(&mut tokens, &mut word, &mut word_started);
                        break;
                    }
                }
            }
            '\n' => push_boundary(&mut tokens, &mut word, &mut word_started),
            ch if ch.is_whitespace() => push_word(&mut tokens, &mut word, &mut word_started),
            ';' | '(' | ')' => push_boundary(&mut tokens, &mut word, &mut word_started),
            '&' | '|' => {
                push_boundary(&mut tokens, &mut word, &mut word_started);
                if chars.peek() == Some(&ch) {
                    chars.next();
                }
            }
            _ => {
                word_started = true;
                word.push(ch);
            }
        }
    }
    push_word(&mut tokens, &mut word, &mut word_started);
    tokens
}

fn push_word(tokens: &mut Vec<ShellToken>, word: &mut String, word_started: &mut bool) {
    if *word_started {
        tokens.push(ShellToken::Word(std::mem::take(word)));
        *word_started = false;
    }
}

fn push_boundary(tokens: &mut Vec<ShellToken>, word: &mut String, word_started: &mut bool) {
    push_word(tokens, word, word_started);
    if !matches!(tokens.last(), Some(ShellToken::Boundary)) {
        tokens.push(ShellToken::Boundary);
    }
}

impl HookInput {
    /// The LIVE background tasks that are a `clank run` carrying this
    /// description.
    ///
    /// The caller wants exactly one. Zero proves nothing is running.
    /// Two or more means the description cannot tell them apart, and
    /// picking either would be a guess about whether an agent may be
    /// silenced — so both cases must decline to suppress. The tasks
    /// themselves are returned, not a count, because the unique match
    /// carries the one thing a `clank run` marker cannot: the harness
    /// task id the agent would need to re-attend it.
    pub fn live_clank_runs(&self, desc: &str) -> Vec<&BackgroundTask> {
        self.background_tasks
            .iter()
            .filter(|t| t.status.as_deref() != Some("completed"))
            .filter(|t| {
                t.command
                    .as_deref()
                    .is_some_and(|cmd| clank_run_desc(cmd).as_deref() == Some(desc))
            })
            .collect()
    }
}

impl HookInput {
    /// Ids of background tasks the tool reports as live this turn.
    ///
    /// ONE of the three signals an `attending` marker needs, and not
    /// sufficient alone: this list has been observed naming tasks that
    /// had already finished (`a-dead-pid-voids-a-falsely-live-marker`
    /// records a `cargo install` that exited 0, its pid gone from the
    /// process table, still listed by the very next hook). Suppression
    /// also requires a live pid and a token that still holds it.
    pub fn live_task_ids(&self) -> Vec<String> {
        self.background_tasks
            .iter()
            .filter(|t| t.status.as_deref() != Some("completed"))
            .filter_map(|t| t.id.clone())
            .collect()
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
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// `clank attending --clear` removed the check-in this turn was
    /// holding. The attendance was cancelled on purpose, so nothing
    /// watches and nothing should.
    AttendanceCleared,
    /// This hook's session no longer holds the label.
    StaleBinding,
    /// Every item was work the agent is already attending: it has a
    /// LIVE background task recorded, and nothing arrived that it did
    /// not already know. Waking here is what turned one long build
    /// into a dozen identical wakes
    /// (attending-suppresses-standing-wakes).
    AttendingBackgroundTask,
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
    fn clank_run_desc_recognises_the_invocation_not_the_words() {
        assert_eq!(
            clank_run_desc("clank run --desc \"test run\" -- cargo test").as_deref(),
            Some("test run")
        );
        assert_eq!(
            clank_run_desc("/Users/x/.cargo/bin/clank run --desc 'test run' -- make").as_deref(),
            Some("test run")
        );
        assert_eq!(
            clank_run_desc("cd /repo && exec clank run --desc=build -- ./x").as_deref(),
            Some("build")
        );

        // Substring matching is NOT identity: each of these names the
        // same words and is not a run.
        assert_eq!(clank_run_desc("echo \"test run\""), None);
        assert_eq!(clank_run_desc("cargo test # test run"), None);
        assert_eq!(clank_run_desc("clank wait --author claude"), None);
        assert_eq!(
            clank_run_desc("clank attending x --desc \"test run\""),
            None
        );

        // Everything after `--` belongs to the CHILD. Its `--desc` is
        // not clank's and must not be picked up.
        assert_eq!(
            clank_run_desc("clank run --desc \"ours\" -- cargo test --desc \"theirs\"").as_deref(),
            Some("ours")
        );
        assert_eq!(
            clank_run_desc("clank run -- cargo test --desc \"theirs\""),
            None,
            "a run with no --desc of its own matches nothing"
        );
    }

    #[test]
    fn live_run_matches_counts_only_live_runs_with_that_exact_description() {
        let input = |tasks: serde_json::Value| -> HookInput {
            serde_json::from_value(serde_json::json!({
                "session_id": "sess-run-match",
                "cwd": "/tmp",
                "stop_hook_active": false,
                "background_tasks": tasks,
            }))
            .unwrap()
        };
        let run = |cmd: &str| serde_json::json!({"id": "t", "command": cmd});

        let one = input(serde_json::json!([run(
            "clank run --desc \"test run\" -- cargo test"
        )]));
        assert_eq!(one.live_clank_runs("test run").len(), 1);
        assert_eq!(
            one.live_clank_runs("test run")[0].id.as_deref(),
            Some("t"),
            "the unique match carries the harness id the marker cannot"
        );
        assert_eq!(one.live_clank_runs("other").len(), 0);

        // A finished task is not evidence that anything is running.
        let done = input(serde_json::json!([{
            "id": "t",
            "status": "completed",
            "command": "clank run --desc \"test run\" -- cargo test"
        }]));
        assert_eq!(done.live_clank_runs("test run").len(), 0);

        // Two live runs sharing a description cannot be told apart, so
        // the caller's "exactly one" rule declines to suppress rather
        // than picking either.
        let two = input(serde_json::json!([
            run("clank run --desc \"test run\" -- cargo test"),
            run("clank run --desc \"test run\" -- cargo build"),
        ]));
        assert_eq!(two.live_clank_runs("test run").len(), 2);
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
