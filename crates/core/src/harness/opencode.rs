//! OpenCode, through clank's own plugin
//! (`crates/cli/src/cli/setup_assets/opencode_plugin.js`). On a session's
//! idle the plugin runs the Stop hook as a long-poll; non-empty stdout is a
//! prompt, and the renewal wire asks the plugin to poll again instead.

use super::{Adapter, Answer, Behaviour, Deadline, Protocol, Stop, Stream};
use crate::hook_io::{HOOK_OK_EXIT, HookOutcome};
use crate::vocab::Tool;

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::OpenCode,
    name: "OpenCode",
    protocol: Protocol::Common,
    session_env: "OPENCODE_SESSION_ID",
    start_sends_transcript: false,
    behaviour: Behaviour {
        stop: Stop::PluginPoll,
        deadline: Deadline::RenewsPoll,
        lists_background_tasks: false,
        reports_background_completion: false,
        session_start_reaches_model: false,
    },
    recorded: include_str!("../../tests/fixtures/opencode/manifest.json"),
    answer,
    classify: Some(failure_class),
};

/// Intercepted by the plugin before its non-empty stdout would be taken
/// as a model prompt.
pub const RENEW_WAIT_WIRE: &str = r#"{"clank":"renew_wait"}"#;

fn answer(outcome: &HookOutcome) -> Answer {
    match outcome {
        HookOutcome::Continue { reason } => {
            Answer::said(Stream::Stdout, reason.as_str(), HOOK_OK_EXIT, true)
        }
        _ => Answer::said(Stream::Stdout, RENEW_WAIT_WIRE, HOOK_OK_EXIT, false),
    }
}

/// 2.x names a failed execution's error by a `provider.*` type (1.x by an
/// error class), and a generic error by its HTTP status.
fn failure_class(name: &str, status: Option<u16>) -> &'static str {
    match (name, status) {
        ("ProviderAuthError" | "provider.auth", _) => "authentication_failed",
        ("MessageOutputLengthError", _) => "max_output_tokens",
        ("provider.rate-limit", _) => "rate_limit",
        ("provider.quota" | "provider.free-limit", _) => "usage_limit_exceeded",
        ("provider.invalid-request" | "provider.content-filter", _) => "invalid_request",
        ("provider.no-route" | "provider.unknown", _) => "model_not_found",
        ("APIError" | "provider.error" | "provider.internal", Some(429)) => "rate_limit",
        ("APIError" | "provider.error" | "provider.internal", Some(529 | 503)) => "overloaded",
        ("APIError" | "provider.error", Some(500..=599)) => "server_error",
        ("provider.internal" | "provider.transport" | "provider.timeout", _) => "server_error",
        ("APIError" | "provider.error", Some(401 | 403)) => "authentication_failed",
        ("APIError" | "provider.error", Some(400 | 404 | 413 | 422)) => "invalid_request",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::parse_stop;
    use crate::turn_policy::TaskSnapshot;

    /// Synthetic: the plugin's own Stop, as `opencode_plugin.js` writes it.
    #[test]
    fn the_plugin_s_stop_parses_and_lists_nothing() {
        let input =
            parse_stop(r#"{"session_id":"ses_opencode_a","cwd":"/r","stop_hook_active":false}"#)
                .unwrap();
        assert_eq!(input.session_id.as_str(), "ses_opencode_a");
        assert_eq!(TaskSnapshot::of(&input), TaskSnapshot::Unavailable);
    }

    #[test]
    fn a_continuation_is_the_prompt_on_stdout_and_a_renewal_its_wire() {
        assert_eq!(
            ADAPTER.answer(&HookOutcome::Continue {
                reason: "work".into()
            }),
            Answer::said(Stream::Stdout, "work", HOOK_OK_EXIT, true)
        );
        assert_eq!(
            ADAPTER.answer(&HookOutcome::RenewWait),
            Answer::said(Stream::Stdout, RENEW_WAIT_WIRE, HOOK_OK_EXIT, false)
        );
    }

    /// Synthetic: the error names the plugin reports, from OpenCode 1.x's
    /// classes and 2.x's `provider.*` types.
    #[test]
    fn opencode_errors_map_to_the_classes_retries_know() {
        let class = |name, status| ADAPTER.failure_class(name, status);
        assert_eq!(class("APIError", Some(529)), "overloaded");
        assert_eq!(class("APIError", Some(500)), "server_error");
        assert_eq!(class("APIError", Some(429)), "rate_limit");
        assert_eq!(class("ProviderAuthError", None), "authentication_failed");
        assert_eq!(class("MessageOutputLengthError", None), "max_output_tokens");
        assert_eq!(class("UnknownError", None), "unknown");
        assert_eq!(class("provider.auth", None), "authentication_failed");
        assert_eq!(class("provider.rate-limit", Some(429)), "rate_limit");
        assert_eq!(class("provider.internal", Some(529)), "overloaded");
        assert_eq!(class("provider.internal", Some(500)), "server_error");
        assert_eq!(class("provider.transport", None), "server_error");
        assert_eq!(class("provider.quota", None), "usage_limit_exceeded");
        assert_eq!(class("provider.error", Some(404)), "invalid_request");
        assert_eq!(class("provider.invalid-output", None), "unknown");
        assert_eq!(
            crate::harness::of(Tool::Codex).failure_class("overloaded", None),
            "overloaded",
            "a harness that names its own classes keeps them"
        );
    }
}
