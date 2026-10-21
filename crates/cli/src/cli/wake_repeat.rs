//! What a repeated delivery says, and where a Stop sits in the agent's own
//! record, which is what repeat counting places it by.
//!
//! The count itself is the monitor's ([`clank_core::turn_policy`]):
//! repeated delivery is a fact about delivery, not proof the agent did
//! nothing, so it escalates only the wording of the message and grants no
//! authority over the user's instructions.
//!
//! What each adapter can tell clank:
//! - **Claude** records each turn, and its origin, in its transcript. A
//!   Stop is placed by its newest turn record, so a repeated or replayed
//!   invocation is not counted again, and a person's own turn resets the
//!   count.
//! - **Codex** records its turn events in its rollout but not their origin.
//!   A Stop is placed by its newest turn event, so repeats are not counted
//!   again, and nothing resets on a turn.
//!
//! Placement assumes an append-only record: a record that is replaced or
//! compacted can make a new Stop look old until the items change.
//! - **OpenCode** names neither: each continuation counts, and nothing
//!   resets on a turn.
//! - **Grok** hands work over through its own wait, not this hook's
//!   continuation, and is not counted.

use std::path::Path;

use clank_core::turn_policy::ESCALATE_AT;

/// The paragraph the hook adds once the same items have been delivered
/// [`ESCALATE_AT`] times: the repetition, the limits it does not lift,
/// and `duties`, one line per item saying what discharges it.
pub(crate) fn escalation(count: u32, duties: &[String]) -> Option<String> {
    (count >= ESCALATE_AT).then(|| {
        let mut text = format!(
            "\n\nThese items have been delivered to you {count} times unchanged. Ending your \
             turn does not discharge them: they stay owed and will be delivered again. Follow \
             the user's latest instruction, any explicit pause, and the actual reason given for \
             any rejected action; repeated delivery grants no new authority. If you are \
             waiting on background work, attend it rather than ending your turn to wait: \
             `clank attend <name>` for a subagent, `clank run` for shell work."
        );
        if !duties.is_empty() {
            text.push_str(" What discharges each:");
            for duty in duties {
                text.push('\n');
                text.push_str(duty);
            }
        }
        text
    })
}

/// Where a Stop comes from in the agent's own record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Boundary {
    pub(crate) turn: String,
    /// The offset of the record that placed it.
    pub(crate) position: u64,
    /// The harness recorded the turn as a person's.
    pub(crate) human: bool,
}

/// How far back a record is read for its newest boundary: a boundary
/// further back is unknown, never guessed.
const WINDOW: u64 = 256 * 1024;

/// A Claude transcript's newest turn, placed at its own prompt record: its
/// `promptId`, and whether Claude recorded it as a person's (`origin.kind`
/// of `human`). A tool result shares its turn's `promptId` and is never a
/// turn's start, so a turn whose prompt lies further back than the window
/// is unknown, not placed at a later fragment of it.
pub(crate) fn claude_boundary(transcript: &Path) -> Option<Boundary> {
    let (_, lines) = crate::cli::web::transcript::Tail::open(transcript, WINDOW).ok()?;
    let mut newest: Option<String> = None;
    let mut started: Option<Boundary> = None;
    for line in &lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line.text) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let Some(id) = v.get("promptId").and_then(|p| p.as_str()) else {
            continue;
        };
        if newest.as_deref() != Some(id) {
            newest = Some(id.to_string());
        }
        let prompt = !v
            .pointer("/message/content")
            .and_then(|c| c.as_array())
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            });
        if prompt && started.as_ref().is_none_or(|b| b.turn != id) {
            started = Some(Boundary {
                turn: id.to_string(),
                position: line.offset,
                human: v.pointer("/origin/kind").and_then(|k| k.as_str()) == Some("human"),
            });
        }
    }
    started.filter(|b| newest.as_deref() == Some(b.turn.as_str()))
}

/// The newest event of Codex turn `turn` in its rollout, which a Stop of
/// that turn follows: the user's or agent's message, or the turn's start,
/// end or abort. Messages belong to the turn whose start precedes them.
/// Other appends (token counts, tool output) do not move it, and a turn
/// whose start is not in the window is unknown, not placed at another
/// turn's events.
pub(crate) fn codex_boundary(rollout: &Path, turn: &str) -> Option<Boundary> {
    const MESSAGES: [&str; 2] = ["user_message", "agent_message"];
    const OWN: [&str; 3] = ["task_started", "task_complete", "turn_aborted"];
    let (_, lines) = crate::cli::web::transcript::Tail::open(rollout, WINDOW).ok()?;
    let mut current: Option<String> = None;
    let mut newest = None;
    for line in &lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line.text) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        let Some(kind) = v.pointer("/payload/type").and_then(|k| k.as_str()) else {
            continue;
        };
        let named = v.pointer("/payload/turn_id").and_then(|id| id.as_str());
        if kind == "task_started" {
            current = named.map(str::to_string);
        }
        let of = if OWN.contains(&kind) {
            named
        } else if MESSAGES.contains(&kind) {
            named.or(current.as_deref())
        } else {
            continue;
        };
        if of == Some(turn) {
            newest = Some(line.offset);
        }
    }
    newest.map(|position| Boundary {
        turn: turn.to_string(),
        position,
        human: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wording_escalates_at_the_threshold_with_its_limits_and_duties() {
        let duties = vec!["- message m1: handle it".to_string()];
        assert_eq!(escalation(ESCALATE_AT - 1, &duties), None);
        let text = escalation(ESCALATE_AT, &duties).unwrap();
        assert!(text.contains(&format!("{ESCALATE_AT} times unchanged")));
        for kept in [
            "user's latest instruction",
            "explicit pause",
            "reason given for any rejected action",
            "grants no new authority",
        ] {
            assert!(text.contains(kept), "{kept}");
        }
        assert!(text.ends_with("\n- message m1: handle it"));
        assert!(
            !text.contains("clank block"),
            "a block is the duty's to offer"
        );
        assert!(text.contains("`clank attend <name>`"), "{text}");
        assert!(text.contains("`clank run`"), "{text}");
    }

    /// A person's typed prompt is human; a hook wake, a peer's message,
    /// clank's own recovery wake, a tool result and anything unmarked are
    /// not. The boundary is where the newest turn begins.
    #[test]
    fn a_claude_turn_is_a_persons_only_when_claude_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let user = |id: &str, origin: Option<&str>, text: &str| {
            let mut v = serde_json::json!({"type":"user","promptId":id,"message":{"content":text}});
            if let Some(kind) = origin {
                v["origin"] = serde_json::json!({ "kind": kind });
            }
            v.to_string()
        };
        let latest = |lines: &[String]| {
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            claude_boundary(&path).map(|b| (b.turn, b.human, b.position))
        };
        assert_eq!(
            latest(&[user("p1", Some("human"), "please stop")]),
            Some(("p1".into(), true, 0))
        );
        for (origin, text) in [
            (
                Some("task-notification"),
                "Clank wait returned work for `claude`",
            ),
            (Some("peer"), "Another Claude session sent a message"),
            (
                None,
                &*crate::cli::recovery::wake_text("clank-recovery-t1-1"),
            ),
            (None, "[Request interrupted by user]"),
        ] {
            let first = user("p1", Some("human"), "earlier");
            let at = first.len() as u64 + 1;
            assert_eq!(
                latest(&[first, user("p2", origin, text)]),
                Some(("p2".into(), false, at)),
                "{origin:?} {text}"
            );
        }
        let tool_result =
            r#"{"type":"user","promptId":"p1","message":{"content":[{"type":"tool_result"}]}}"#;
        assert_eq!(
            latest(&[user("p1", Some("human"), "go"), tool_result.into()]),
            Some(("p1".into(), true, 0)),
            "a tool result belongs to its turn, and does not move it"
        );
        assert_eq!(latest(&[]), None);
    }

    /// A Codex Stop is placed by its own turn's newest event: bookkeeping
    /// does not move it, the agent speaking again does, and a replayed
    /// Stop of an earlier turn stays where that turn ended.
    #[test]
    fn a_codex_stop_is_placed_within_its_own_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let event = |kind: &str, turn: Option<&str>| match turn {
            Some(id) => {
                format!(r#"{{"type":"event_msg","payload":{{"type":"{kind}","turn_id":"{id}"}}}}"#)
            }
            None => format!(r#"{{"type":"event_msg","payload":{{"type":"{kind}"}}}}"#),
        };
        let at = |turn: &str| codex_boundary(&path, turn).map(|b| b.position);
        use std::io::Write as _;
        std::fs::write(&path, "").unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let mut put = |line: String| {
            let offset = f.metadata().unwrap().len();
            writeln!(f, "{line}").unwrap();
            offset
        };
        put(event("task_started", Some("old")));
        let old_said = put(event("agent_message", None));
        put(event("token_count", None));
        assert_eq!(at("old"), Some(old_said), "bookkeeping does not move it");
        let old_end = put(event("task_complete", Some("old")));
        put(event("task_started", Some("new")));
        let new_said = put(event("agent_message", None));
        assert_eq!(
            at("old"),
            Some(old_end),
            "the old turn stays where it ended"
        );
        assert_eq!(at("new"), Some(new_said));
        assert_eq!(at("never"), None, "a turn not in the record is unknown");
    }

    /// A Claude turn whose own prompt is further back than the window is
    /// unknown, not placed at a later tool result of the same turn.
    #[test]
    fn a_claude_turn_is_placed_only_at_its_own_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let prompt = r#"{"type":"user","promptId":"p1","origin":{"kind":"human"},"message":{"content":"go"}}"#;
        let big = format!(
            r#"{{"type":"user","promptId":"p1","message":{{"content":[{{"type":"tool_result","content":"{}"}}]}}}}"#,
            "x".repeat(WINDOW as usize + 1024)
        );
        std::fs::write(&path, format!("{prompt}\n{big}\n")).unwrap();
        assert_eq!(claude_boundary(&path), None);
        std::fs::write(&path, format!("{prompt}\n")).unwrap();
        assert_eq!(claude_boundary(&path).map(|b| b.position), Some(0));
    }
}
