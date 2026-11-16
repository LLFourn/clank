//! Grok's native lifecycle. SessionStart binds, SessionEnd releases, and a
//! Stop that ends a turn is a decision gate clank never parks: blocking it
//! with a reason continues the turn, here with the request to run a
//! background `clank wait`, which Grok tells the agent the end of.

use std::path::PathBuf;

use serde::Deserialize;

use super::{Adapter, Answer, Behaviour, Deadline, Protocol, Stop, Stream};
use crate::hook_io::{BackgroundTask, HOOK_OK_EXIT, HookOutcome};
use crate::ids::SessionId;
use crate::vocab::Tool;

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Grok,
    name: "Grok",
    protocol: Protocol::GrokNative,
    session_env: "GROK_AGENT",
    start_sends_transcript: false,
    behaviour: Behaviour {
        stop: Stop::Handoff,
        deadline: Deadline::NeverParks,
        lists_background_tasks: true,
        reports_background_completion: true,
        session_start_reaches_model: false,
    },
    recorded: include_str!("../../tests/fixtures/grok/manifest.json"),
    answer,
    classify: None,
};

/// Set on every hook Grok runs, the Claude hook files it also imports
/// included: a clank hook of any other protocol seeing it is running under
/// Grok, whose native hook owns the lifecycle.
pub const HOOK_EVENT_VAR: &str = "GROK_HOOK_EVENT";

fn answer(outcome: &HookOutcome) -> Answer {
    match outcome {
        HookOutcome::Continue { reason } => {
            Answer::said(Stream::Stdout, block(reason), HOOK_OK_EXIT, true)
        }
        _ => Answer::unrenewable(),
    }
}

/// A Stop blocked with `reason`, which continues the turn with it.
pub fn block(reason: &str) -> String {
    serde_json::json!({
        "decision": "block",
        "reason": reason,
    })
    .to_string()
}

/// One native hook event.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub hook_event_name: String,
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub reason: Option<String>,
    pub subagent_type: Option<String>,
    #[serde(default)]
    pub stop_hook_active: bool,
    /// `None` when the payload omits it, which is no listing, never an
    /// empty one.
    #[serde(default)]
    pub background_tasks: Option<Vec<BackgroundTask>>,
}

/// What a native event means to clank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Any event about a subagent, which clank leaves alone.
    Subagent,
    SessionStart,
    SessionEnd,
    /// A main-session Stop that ended a turn.
    TurnEnd,
    Other,
}

impl Event {
    pub fn kind(&self) -> Kind {
        if self.subagent_type.is_some() {
            return Kind::Subagent;
        }
        match self.hook_event_name.as_str() {
            "session_start" => Kind::SessionStart,
            "session_end" => Kind::SessionEnd,
            "stop" if self.reason.as_deref() == Some("end_turn") => Kind::TurnEnd,
            _ => Kind::Other,
        }
    }
}

pub fn parse(raw: &[u8]) -> Result<Event, serde_json::Error> {
    serde_json::from_slice(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_policy::TaskSnapshot;

    fn event(json: serde_json::Value) -> Event {
        parse(json.to_string().as_bytes()).unwrap()
    }

    /// Synthetic: no Grok payload has been recorded. These follow the hook
    /// documentation at grok-build 2bdd1d6 (Grok 1.0.46): camelCase fields,
    /// `reason: end_turn` on a turn's Stop, `subagentType` on a subagent's.
    #[test]
    fn each_native_event_reads_as_its_kind() {
        let base = |name: &str| {
            serde_json::json!({"hookEventName": name,
                "sessionId": "11111111-1111-4111-8111-111111111111", "cwd": "/r"})
        };
        assert_eq!(event(base("session_start")).kind(), Kind::SessionStart);
        assert_eq!(event(base("session_end")).kind(), Kind::SessionEnd);
        let mut stop = base("stop");
        assert_eq!(event(stop.clone()).kind(), Kind::Other, "no reason");
        stop["reason"] = "end_turn".into();
        assert_eq!(event(stop.clone()).kind(), Kind::TurnEnd);
        stop["subagentType"] = "explore".into();
        assert_eq!(event(stop).kind(), Kind::Subagent);
    }

    /// Synthetic, as above: a Stop's `backgroundTasks` is its listing, and
    /// an omitted one is no listing.
    #[test]
    fn a_stop_s_listing_is_kept_and_an_omitted_one_is_none() {
        let listed = event(serde_json::json!({"hookEventName": "stop",
            "sessionId": "ses_grok_one", "cwd": "/r", "reason": "end_turn", "stopHookActive": true,
            "backgroundTasks": [{"id": "g1", "type": "subagent", "status": "running",
                "description": "sleeper"}]}));
        assert!(listed.stop_hook_active);
        let TaskSnapshot::Known(tasks) = TaskSnapshot::listed(listed.background_tasks) else {
            panic!("a listing")
        };
        assert_eq!(tasks[0].description.as_deref(), Some("sleeper"));
        let bare = event(
            serde_json::json!({"hookEventName": "stop", "sessionId": "ses_grok_one",
            "cwd": "/r", "reason": "end_turn"}),
        );
        assert_eq!(
            TaskSnapshot::listed(bare.background_tasks),
            TaskSnapshot::Unavailable
        );
    }

    #[test]
    fn a_continuation_blocks_the_stop_on_stdout() {
        assert_eq!(
            ADAPTER.answer(&HookOutcome::Continue {
                reason: "arm".into()
            }),
            Answer::said(
                Stream::Stdout,
                r#"{"decision":"block","reason":"arm"}"#,
                HOOK_OK_EXIT,
                true
            )
        );
    }
}
