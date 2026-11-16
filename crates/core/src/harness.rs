//! One adapter per harness: the only place on the wake path that knows how a
//! tool's hooks speak. An [`Adapter`] declares its harness's facts as data,
//! parses the harness's hook payloads into clank's types, and renders
//! clank's answers into the harness's output shape and exit code. Pure:
//! payloads in, facts and answers out.
//!
//! A recorded payload proves parsing, not behaviour. What a harness DOES
//! (re-wakes, holds the turn, reports a run's end) is [`Behaviour`], and
//! every value of it has a provenance entry in `docs/harness-support.toml`
//! saying whether it was observed, read from source, chosen as policy or is
//! unknown; the `harness_support` gate fails on one without.

use serde::{Deserialize, Serialize};

use crate::hook_io::{HOOK_OK_EXIT, HookInput, HookOutcome};
use crate::turn_policy::DeadlineVerdict;
use crate::vocab::Tool;

pub mod claude;
pub mod codex;
pub mod grok;
pub mod opencode;

/// What clank knows of one harness's hooks.
#[derive(Debug)]
pub struct Adapter {
    pub tool: Tool,
    /// The harness's name in `docs/harness-support.toml`.
    pub name: &'static str,
    pub protocol: Protocol,
    /// The variable a hook's `clank wait` child is told its session by.
    pub session_env: &'static str,
    /// The SessionStart payload names the session's transcript.
    pub start_sends_transcript: bool,
    pub behaviour: Behaviour,
    /// The manifest of payloads recorded from real runs, as JSON.
    pub recorded: &'static str,
    answer: fn(&HookOutcome) -> Answer,
    classify: Option<fn(&str, Option<u16>) -> &'static str>,
}

pub fn of(tool: Tool) -> &'static Adapter {
    match tool {
        Tool::Claude => &claude::ADAPTER,
        Tool::Codex => &codex::ADAPTER,
        Tool::Grok => &grok::ADAPTER,
        Tool::OpenCode => &opencode::ADAPTER,
    }
}

pub fn all() -> [&'static Adapter; 4] {
    [
        &claude::ADAPTER,
        &codex::ADAPTER,
        &grok::ADAPTER,
        &opencode::ADAPTER,
    ]
}

/// Which wire a harness's hooks are written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// A [`HookInput`] on stdin, answered by exit code and output. Claude
    /// defines it; Codex and clank's OpenCode plugin speak it too.
    Common,
    /// Grok's native lifecycle events ([`grok::Event`]).
    GrokNative,
}

/// What a harness does, as opposed to what its payloads look like. Each
/// field is a claim with a provenance entry in `docs/harness-support.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Behaviour {
    pub stop: Stop,
    pub deadline: Deadline,
    /// The Stop payload lists the harness's own live background work.
    pub lists_background_tasks: bool,
    /// The harness itself continues a parked agent when its background work
    /// ends. Where it does not, only clank can tell the agent a `clank run`
    /// ended.
    pub reports_background_completion: bool,
    /// What a SessionStart hook answers reaches the model.
    pub session_start_reaches_model: bool,
}

/// How a harness runs clank's Stop hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// The hook runs beside an ended turn, and its continuation wakes the
    /// session again.
    AsyncRewake,
    /// The hook holds the turn until it answers.
    Blocking,
    /// clank's plugin long-polls on the session's idle, renewing the poll at
    /// its deadline.
    PluginPoll,
    /// A decision gate clank never parks: the agent is asked to run its own
    /// background `clank wait`.
    Handoff,
}

/// What ends a parked Stop's wait, and so what its deadline means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Deadline {
    /// The harness ends the hook; the hook sets no deadline of its own.
    HarnessEnds,
    /// The hook's poll has a deadline under the runner's ceiling, at which
    /// the plugin renews it without a model turn.
    RenewsPoll,
    /// The Stop never parks.
    NeverParks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// A hook's answer on its harness's wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The line to write, without its newline.
    pub write: Option<(Stream, String)>,
    pub exit: i32,
    /// Whether the line, once flushed, has reached the agent's channel.
    pub presented: bool,
}

impl Answer {
    fn quiet() -> Self {
        Answer {
            write: None,
            exit: HOOK_OK_EXIT,
            presented: false,
        }
    }

    fn said(stream: Stream, text: impl Into<String>, exit: i32, presented: bool) -> Self {
        Answer {
            write: Some((stream, text.into())),
            exit,
            presented,
        }
    }

    /// The answers every harness gives alike: nothing for a silence, and a
    /// diagnostic on stderr. `None` for the outcomes a harness frames itself.
    fn common(outcome: &HookOutcome) -> Option<Self> {
        match outcome {
            HookOutcome::Silent { .. } => Some(Answer::quiet()),
            HookOutcome::Diagnostic { message } => Some(Answer::said(
                Stream::Stderr,
                message.as_str(),
                HOOK_OK_EXIT,
                false,
            )),
            HookOutcome::Continue { .. } | HookOutcome::RenewWait => None,
        }
    }

    /// A renewal on a harness whose plugin cannot renew.
    fn unrenewable() -> Self {
        Answer::said(
            Stream::Stderr,
            "hook: renew-wait action reached an unsupported adapter",
            HOOK_OK_EXIT,
            false,
        )
    }
}

impl Adapter {
    /// `outcome` on this harness's wire.
    pub fn answer(&self, outcome: &HookOutcome) -> Answer {
        Answer::common(outcome).unwrap_or_else(|| (self.answer)(outcome))
    }

    /// What the hook is told when its deadline arrives while parked.
    pub fn deadline_verdict(&self) -> DeadlineVerdict {
        match self.behaviour.deadline {
            Deadline::HarnessEnds => DeadlineVerdict::ReArm,
            Deadline::RenewsPoll => DeadlineVerdict::RenewWait,
            Deadline::NeverParks => {
                DeadlineVerdict::Failed(self.no_in_hook_wait().unwrap_or_default())
            }
        }
    }

    /// Why a report that would park is refused on a harness whose Stop
    /// hands off rather than holding a wait; `None` where it may park.
    pub fn no_in_hook_wait(&self) -> Option<String> {
        (self.behaviour.stop == Stop::Handoff).then(|| {
            format!(
                "hook: {} native handoffs do not use the in-hook wait route",
                self.name
            )
        })
    }

    /// A failure the harness reported by `name` (and HTTP `status`), in the
    /// classes recovery retries by.
    pub fn failure_class(&self, name: &str, status: Option<u16>) -> String {
        match self.classify {
            Some(classify) => classify(name, status).to_string(),
            None => name.to_string(),
        }
    }

    /// The payloads recorded from this harness's real runs.
    pub fn manifest(&self) -> Result<Manifest, String> {
        serde_json::from_str(self.recorded)
            .map_err(|e| format!("{}: the fixture manifest does not parse: {e}", self.name))
    }
}

/// A Stop, PostCompact, SubagentStop or SubagentStart in the
/// [`Protocol::Common`] wire.
pub fn parse_stop(raw: &str) -> Result<HookInput, String> {
    serde_json::from_str(raw).map_err(|e| format!("hook: parsing stdin JSON: {e}"))
}

/// A SessionStart in the [`Protocol::Common`] wire.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionStart {
    pub session_id: String,
    pub cwd: String,
    /// `startup` / `resume` / `clear` / `compact`.
    #[serde(default)]
    pub source: Option<String>,
    /// The session's own transcript, where the harness sends it.
    #[serde(default)]
    pub transcript_path: Option<String>,
}

pub fn parse_session_start(raw: &str) -> Result<SessionStart, String> {
    serde_json::from_str(raw).map_err(|e| format!("hook: parsing SessionStart JSON: {e}"))
}

/// A SessionStart's answer in the [`Protocol::Common`] wire: `context`
/// added to the session's context.
pub fn session_start_answer(context: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": context,
        }
    })
    .to_string()
}

/// The event names, wire keys and variables the adapters own. Outside
/// core, wake-path code that spells one is speaking a harness's protocol
/// itself; the `harness_adapter_boundary` gate refuses it.
pub const PROTOCOL_WORDS: &[&str] = &[
    "hookSpecificOutput",
    "hookEventName",
    "hook_event_name",
    "additionalContext",
    "decision",
    "SessionStart",
    "SessionEnd",
    "session_start",
    "session_end",
    "end_turn",
    "Stop",
    "SubagentStop",
    "SubagentStart",
    "PostCompact",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "AskUserQuestion",
    "stop_hook_active",
    "stopHookActive",
    "background_tasks",
    "backgroundTasks",
    "tool_use_id",
    "tool_name",
    "tool_input",
    "transcript_path",
    "turn_id",
    "agent_id",
    grok::HOOK_EVENT_VAR,
    opencode::RENEW_WAIT_WIRE,
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "OPENCODE_SESSION_ID",
    "GROK_AGENT",
];

/// The version a recording carries when the run that made it did not say.
pub const UNKNOWN_VERSION: &str = "unknown";

/// The payloads under `crates/core/tests/fixtures/<tool>/` recorded from a
/// real run, each with the harness version that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub tool: String,
    /// What a reader needs to know of the whole set, such as that nothing
    /// has been recorded.
    pub note: String,
    pub recorded: Vec<Recording>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    pub file: String,
    pub hook: String,
    /// The harness's version, or [`UNKNOWN_VERSION`].
    pub version: String,
    /// YYYY-MM-DD.
    pub date: String,
    /// How the version was established, or why it could not be.
    pub provenance: String,
}

impl Manifest {
    /// The recordings that cannot be vouched for on `installed`: made with
    /// another version, or with one nobody wrote down.
    pub fn drifted(&self, installed: &str) -> Vec<&Recording> {
        self.recorded
            .iter()
            .filter(|r| r.version != installed)
            .collect()
    }
}

/// The `x.y.z` a `--version` line names, wherever in the line it stands.
pub fn version_in(output: &str) -> Option<&str> {
    output
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .map(|word| word.trim_start_matches('v'))
        .find(|word| {
            let parts: Vec<&str> = word.split('.').collect();
            parts.len() >= 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        })
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// The payload recorded as `file` for `tool`, as bytes on stdin.
    pub(crate) fn recorded(tool: &str, file: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/{tool}/{file}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every manifest parses, names its own tool, lists exactly the payload
    /// files beside it, and dates each recording.
    #[test]
    fn each_manifest_lists_exactly_its_recordings() {
        for adapter in all() {
            let manifest = adapter.manifest().unwrap();
            assert_eq!(manifest.tool, adapter.tool.as_str());
            let dir = format!(
                "{}/tests/fixtures/{}",
                env!("CARGO_MANIFEST_DIR"),
                adapter.tool.as_str()
            );
            let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name != "manifest.json")
                .collect();
            on_disk.sort();
            let mut listed: Vec<String> =
                manifest.recorded.iter().map(|r| r.file.clone()).collect();
            listed.sort();
            assert_eq!(listed, on_disk, "{dir}");
            for r in &manifest.recorded {
                assert!(!r.version.is_empty() && !r.provenance.is_empty(), "{r:?}");
                let b = r.date.as_bytes();
                assert!(
                    b.len() == 10 && b[4] == b'-' && b[7] == b'-',
                    "{}: {}",
                    r.file,
                    r.date
                );
            }
        }
    }

    #[test]
    fn a_recording_drifts_unless_made_with_the_installed_version() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"tool":"claude","note":"n","recorded":[
                {"file":"a.json","hook":"Stop","version":"2.1.308","date":"2026-11-11","provenance":"p"},
                {"file":"b.json","hook":"Stop","version":"unknown","date":"2026-10-21","provenance":"p"}]}"#,
        )
        .unwrap();
        let files = |installed| -> Vec<&str> {
            manifest
                .drifted(installed)
                .iter()
                .map(|r| r.file.as_str())
                .collect()
        };
        assert_eq!(files("2.1.308"), ["b.json"]);
        assert_eq!(files("2.1.400"), ["a.json", "b.json"]);
    }

    #[test]
    fn the_version_is_read_from_each_harness_s_line() {
        assert_eq!(version_in("2.1.308 (Claude Code)\n"), Some("2.1.308"));
        assert_eq!(version_in("codex-cli 0.167.2\n"), Some("0.167.2"));
        assert_eq!(version_in("1.18.30"), Some("1.18.30"));
        assert_eq!(version_in("grok v1.0.46"), Some("1.0.46"));
        assert_eq!(version_in("no version here"), None);
        assert_eq!(version_in("build 7"), None);
    }

    /// Silence and diagnostics read the same on every harness: nothing, or
    /// the message on stderr, and never a non-zero exit.
    #[test]
    fn silence_and_diagnostics_are_alike_everywhere() {
        for adapter in all() {
            assert_eq!(
                adapter.answer(&HookOutcome::Silent {
                    why: crate::SilentReason::AutoOff
                }),
                Answer::quiet()
            );
            assert_eq!(
                adapter.answer(&HookOutcome::Diagnostic {
                    message: "m".into()
                }),
                Answer::said(Stream::Stderr, "m", HOOK_OK_EXIT, false)
            );
        }
    }

    #[test]
    fn a_deadline_means_what_the_harness_s_stop_makes_it() {
        assert_eq!(of(Tool::Claude).deadline_verdict(), DeadlineVerdict::ReArm);
        assert_eq!(of(Tool::Codex).deadline_verdict(), DeadlineVerdict::ReArm);
        assert_eq!(
            of(Tool::OpenCode).deadline_verdict(),
            DeadlineVerdict::RenewWait
        );
        assert_eq!(
            of(Tool::Grok).deadline_verdict(),
            DeadlineVerdict::Failed(
                "hook: Grok native handoffs do not use the in-hook wait route".into()
            )
        );
    }

    #[test]
    fn a_session_start_answer_carries_its_context_as_additional_context() {
        let v: serde_json::Value =
            serde_json::from_str(&session_start_answer("Pending work\nitems")).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"hookSpecificOutput": {"hookEventName": "SessionStart",
                "additionalContext": "Pending work\nitems"}})
        );
    }

    /// Every protocol word is one an adapter or the common wire really
    /// speaks: each appears in the source that defines the wire.
    #[test]
    fn every_protocol_word_is_spoken_by_core() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let source: String = [
            "harness.rs",
            "harness/claude.rs",
            "harness/codex.rs",
            "harness/grok.rs",
            "harness/opencode.rs",
            "hook_io.rs",
        ]
        .iter()
        .map(|f| std::fs::read_to_string(format!("{dir}/{f}")).unwrap())
        .collect();
        let start = source.find("pub const PROTOCOL_WORDS").unwrap();
        let end = start + source[start..].find("\n];\n").unwrap();
        let without_list = format!("{}{}", &source[..start], &source[end..]);
        for word in PROTOCOL_WORDS {
            assert!(
                without_list.contains(word),
                "{word} is spoken nowhere in core"
            );
        }
    }

    #[test]
    fn each_adapter_is_the_one_for_its_tool() {
        for adapter in all() {
            assert!(std::ptr::eq(of(adapter.tool), adapter));
        }
    }
}
