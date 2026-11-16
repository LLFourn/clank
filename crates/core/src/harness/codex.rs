//! Codex. Its Stop hook is synchronous: it holds the turn, and a
//! `{"decision":"block","reason":…}` on stdout continues the agent with the
//! reason as its next prompt.

use super::{Adapter, Answer, Behaviour, Deadline, Protocol, Stop, Stream};
use crate::hook_io::{CodexBlockDecision, HOOK_OK_EXIT, HookOutcome};
use crate::vocab::Tool;

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Codex,
    name: "Codex",
    protocol: Protocol::Common,
    session_env: "CODEX_THREAD_ID",
    start_sends_transcript: false,
    behaviour: Behaviour {
        stop: Stop::Blocking,
        deadline: Deadline::HarnessEnds,
        lists_background_tasks: false,
        reports_background_completion: false,
        session_start_reaches_model: false,
    },
    recorded: include_str!("../../tests/fixtures/codex/manifest.json"),
    answer,
    classify: None,
};

fn answer(outcome: &HookOutcome) -> Answer {
    match outcome {
        HookOutcome::Continue { reason } => Answer::said(
            Stream::Stdout,
            serde_json::json!(CodexBlockDecision::from_reason(reason)).to_string(),
            HOOK_OK_EXIT,
            true,
        ),
        _ => Answer::unrenewable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::fixtures::recorded;
    use crate::harness::{parse_session_start, parse_stop};
    use crate::turn_policy::TaskSnapshot;

    /// The recorded Stop parses with its turn, and, sending no listing,
    /// is no snapshot rather than an empty one.
    #[test]
    fn the_recorded_stop_names_its_turn_and_lists_nothing() {
        let input = parse_stop(&recorded("codex", "stop_hello.json")).unwrap();
        assert_eq!(
            input.session_id.as_str(),
            "019e5385-ed97-7603-8561-dd9024328ff9"
        );
        assert_eq!(
            input.turn_id.as_deref(),
            Some("019e5386-1684-7250-a7fc-1823f4b8c1be")
        );
        assert!(!input.stop_hook_active);
        assert_eq!(TaskSnapshot::of(&input), TaskSnapshot::Unavailable);
    }

    /// Synthetic: no Codex SessionStart has been recorded. The shape is the
    /// one Codex 0.153's embedded schema declares, with no transcript.
    #[test]
    fn a_session_start_parses_without_a_transcript() {
        let start = parse_session_start(
            r#"{"session_id":"019e5385-ed97-7603-8561-dd9024328ff9","cwd":"/r",
                "hook_event_name":"SessionStart","source":"startup"}"#,
        )
        .unwrap();
        assert_eq!(start.source.as_deref(), Some("startup"));
        assert_eq!(start.transcript_path, None);
    }

    #[test]
    fn a_continuation_is_a_block_decision_on_stdout() {
        assert_eq!(
            ADAPTER.answer(&HookOutcome::Continue {
                reason: "line 1\n\"quoted\"".into()
            }),
            Answer::said(
                Stream::Stdout,
                r#"{"decision":"block","reason":"line 1\n\"quoted\""}"#,
                HOOK_OK_EXIT,
                true
            )
        );
        assert_eq!(
            ADAPTER.answer(&HookOutcome::RenewWait),
            Answer::unrenewable()
        );
    }
}
