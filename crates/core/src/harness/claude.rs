//! Claude Code. Its Stop runs under `asyncRewake` (setup's floor, 2.1.223):
//! the turn ends, the hook parks beside it, and exit 2 wakes the session
//! with stderr as the prompt.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Adapter, Answer, Behaviour, Deadline, Protocol, Stop, Stream};
use crate::hook_io::{CLAUDE_CONTINUATION_EXIT, HookOutcome};
use crate::vocab::Tool;

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Claude,
    name: "Claude Code",
    protocol: Protocol::Common,
    session_env: "CLAUDE_CODE_SESSION_ID",
    start_sends_transcript: true,
    behaviour: Behaviour {
        stop: Stop::AsyncRewake,
        deadline: Deadline::HarnessEnds,
        lists_background_tasks: true,
        reports_background_completion: true,
        session_start_reaches_model: true,
    },
    recorded: include_str!("../../tests/fixtures/claude/manifest.json"),
    answer,
    classify: None,
};

fn answer(outcome: &HookOutcome) -> Answer {
    match outcome {
        HookOutcome::Continue { reason } => Answer::said(
            Stream::Stderr,
            reason.as_str(),
            CLAUDE_CONTINUATION_EXIT,
            true,
        ),
        _ => Answer::unrenewable(),
    }
}

/// Claude's tool hook for a call clank's question hook is installed on:
/// a PreToolUse, PostToolUse, PostToolUseFailure or PermissionRequest.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct QuestionHook {
    pub cwd: String,
    pub session_id: String,
    pub prompt_id: String,
    pub hook_event_name: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: Option<String>,
    pub tool_response: Option<Value>,
}

/// What a [`QuestionHook`] says about an `AskUserQuestion` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question<'a> {
    /// The call is about to ask; `header` is one line saying what.
    Asked { tool_use: &'a str, header: String },
    /// The call returned, answered or failed.
    Returned { tool_use: &'a str },
}

impl QuestionHook {
    /// `None` for another tool, another event, or a call with no id.
    pub fn question(&self) -> Option<Question<'_>> {
        if self.tool_name != "AskUserQuestion" {
            return None;
        }
        let tool_use = self.tool_use_id.as_deref()?;
        match self.hook_event_name.as_str() {
            "PreToolUse" => Some(Question::Asked {
                tool_use,
                header: header_of(&self.tool_input),
            }),
            "PostToolUse" | "PostToolUseFailure" => Some(Question::Returned { tool_use }),
            _ => None,
        }
    }

    /// Claude holds a PermissionRequest until it is decided, so its answer
    /// has no deadline.
    pub fn waits_for_a_decision(&self) -> bool {
        self.hook_event_name == "PermissionRequest"
    }
}

pub fn parse_question_hook(raw: &str) -> Result<QuestionHook, serde_json::Error> {
    serde_json::from_str(raw)
}

/// The first question's header, or else its first line.
fn header_of(input: &Value) -> String {
    let first = input.pointer("/questions/0");
    let pick = |key: &str| {
        first
            .and_then(|q| q.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.lines().next().unwrap_or_default().trim().to_string())
            .filter(|s| !s.is_empty())
    };
    pick("header")
        .or_else(|| pick("question"))
        .unwrap_or_else(|| "a question".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::fixtures::recorded;
    use crate::harness::{parse_session_start, parse_stop};
    use crate::turn_policy::TaskSnapshot;

    fn stop(file: &str) -> crate::HookInput {
        parse_stop(&recorded("claude", file)).unwrap()
    }

    /// Every recorded Stop parses, and its listing is the snapshot clank
    /// reports: a running subagent by id and name, a finished one as an
    /// empty list, and every entry kept whole.
    #[test]
    fn recorded_stops_parse_into_their_listings() {
        let running = stop("stop_subagent_running.json");
        let TaskSnapshot::Known(tasks) = TaskSnapshot::of(&running) else {
            panic!("a listing")
        };
        let [task] = tasks.as_slice() else {
            panic!("{tasks:?}")
        };
        assert_eq!(task.id.as_deref(), Some("a25eee2eef81caf3e"));
        assert_eq!(task.kind.as_deref(), Some("subagent"));
        assert_eq!(task.description.as_deref(), Some("capture-probe sleeper"));
        assert_eq!(task.raw["agent_type"], "general-purpose");
        assert_eq!(
            TaskSnapshot::of(&stop("stop_subagent_finished.json")),
            TaskSnapshot::Known(vec![])
        );
        for file in [
            "stop_monitor_task_running.json",
            "stop_a_parked_b_running.json",
            "stop_a_parked_shell_running.json",
        ] {
            let input = stop(file);
            assert_eq!(input.agent_id, None, "{file}: the agent's own Stop");
            assert!(
                matches!(TaskSnapshot::of(&input), TaskSnapshot::Known(t) if !t.is_empty()),
                "{file}"
            );
        }
    }

    /// Every recorded SubagentStop names its subagent and carries the
    /// session's listing.
    #[test]
    fn recorded_subagent_stops_name_their_subagent() {
        for (file, agent) in [
            ("subagent_stop_a_parks.json", "a2185a1add00531a0"),
            ("subagent_stop_a_final.json", "a2185a1add00531a0"),
            ("subagent_stop_b_finished.json", "aeedcce3823eb5bd2"),
        ] {
            let input = stop(file);
            assert_eq!(input.agent_id.as_deref(), Some(agent), "{file}");
            assert!(input.background_tasks.is_some(), "{file}");
        }
        let parks = stop("subagent_stop_a_parks.json");
        let listed: Vec<_> = parks
            .background_tasks
            .iter()
            .flatten()
            .map(|t| {
                (
                    t.id.as_deref().unwrap(),
                    t.kind.as_deref().unwrap(),
                    t.live(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("sxc82ytp5", "monitor", true),
                ("a2185a1add00531a0", "subagent", true),
                ("aeedcce3823eb5bd2", "subagent", true),
                ("bl2req1k0", "shell", true),
            ],
            "a parking subagent is still listed as running in its own stop"
        );
    }

    /// Synthetic: no SubagentStart or PostCompact has been recorded. These
    /// are the documented shapes: a start names its subagent and lists
    /// nothing; a PostCompact carries the common fields and no
    /// `stop_hook_active`.
    #[test]
    fn a_subagent_start_and_a_post_compact_parse() {
        let started = parse_stop(
            r#"{"session_id":"00000000-0000-0000-0000-000000000000",
                "transcript_path":"/REDACTED/transcript.jsonl","cwd":"/REDACTED/repo",
                "hook_event_name":"SubagentStart","agent_id":"a2185a1add00531a0",
                "agent_type":"general-purpose"}"#,
        )
        .unwrap();
        assert_eq!(started.agent_id.as_deref(), Some("a2185a1add00531a0"));
        assert_eq!(TaskSnapshot::of(&started), TaskSnapshot::Unavailable);
        let compacted = parse_stop(
            r#"{"session_id":"00000000-0000-0000-0000-000000000000",
                "transcript_path":"/REDACTED/transcript.jsonl","cwd":"/REDACTED/repo",
                "hook_event_name":"PostCompact","trigger":"auto"}"#,
        )
        .unwrap();
        assert!(!compacted.stop_hook_active);
    }

    /// Synthetic: no SessionStart has been recorded. Claude's documented
    /// shape carries the session's transcript.
    #[test]
    fn a_session_start_carries_its_transcript() {
        let start = parse_session_start(
            r#"{"session_id":"s1","cwd":"/r","hook_event_name":"SessionStart",
                "source":"resume","transcript_path":"/t.jsonl"}"#,
        )
        .unwrap();
        assert_eq!(start.source.as_deref(), Some("resume"));
        assert_eq!(start.transcript_path.as_deref(), Some("/t.jsonl"));
    }

    fn question_hook(event: &str, tool: &str, id: Option<&str>) -> QuestionHook {
        parse_question_hook(
            &serde_json::json!({"cwd": "/r", "session_id": "s", "prompt_id": "p",
                "hook_event_name": event, "tool_name": tool,
                "tool_input": {"questions": [{"header": "Choose\nmore", "question": "Which?"}]},
                "tool_use_id": id})
            .to_string(),
        )
        .unwrap()
    }

    /// Synthetic: no question hook payload has been recorded here (the
    /// ignored `claude_question_protocol` test drives a real Claude). An
    /// `AskUserQuestion` PreToolUse asks, by its header's first line; its
    /// PostToolUse or PostToolUseFailure returns; anything else, or a call
    /// with no id, says nothing.
    #[test]
    fn question_hooks_classify_by_event_and_tool() {
        assert_eq!(
            question_hook("PreToolUse", "AskUserQuestion", Some("c")).question(),
            Some(Question::Asked {
                tool_use: "c",
                header: "Choose".into()
            })
        );
        for event in ["PostToolUse", "PostToolUseFailure"] {
            assert_eq!(
                question_hook(event, "AskUserQuestion", Some("c")).question(),
                Some(Question::Returned { tool_use: "c" }),
                "{event}"
            );
        }
        assert_eq!(
            question_hook("PermissionRequest", "AskUserQuestion", None).question(),
            None
        );
        assert_eq!(
            question_hook("PreToolUse", "Bash", Some("c")).question(),
            None
        );
        assert_eq!(
            question_hook("PreToolUse", "AskUserQuestion", None).question(),
            None
        );
        let mut bare = question_hook("PreToolUse", "AskUserQuestion", Some("c"));
        bare.tool_input = serde_json::json!({"questions": [{"question": " Which one?\nmore"}]});
        assert!(
            matches!(bare.question(), Some(Question::Asked { header, .. }) if header == "Which one?")
        );
        bare.tool_input = serde_json::json!({});
        assert!(
            matches!(bare.question(), Some(Question::Asked { header, .. }) if header == "a question")
        );
    }

    #[test]
    fn only_a_permission_request_waits_for_its_decision() {
        assert!(question_hook("PermissionRequest", "AskUserQuestion", None).waits_for_a_decision());
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure"] {
            assert!(
                !question_hook(event, "AskUserQuestion", Some("c")).waits_for_a_decision(),
                "{event}"
            );
        }
    }

    #[test]
    fn a_continuation_exits_2_with_the_prompt_on_stderr() {
        assert_eq!(
            ADAPTER.answer(&HookOutcome::Continue {
                reason: "work".into()
            }),
            Answer::said(Stream::Stderr, "work", 2, true)
        );
        assert_eq!(
            ADAPTER.answer(&HookOutcome::RenewWait),
            Answer::unrenewable()
        );
    }
}
