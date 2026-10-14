//! How many times in a row the Stop hook has handed an agent the same items.
//!
//! Repeated delivery is a fact about delivery, not proof the agent did
//! nothing: edits and tests can happen between deliveries before the commit
//! that changes the items. So it is reported as a count, escalates only the
//! wording of the hook's message, and grants no authority over the user's
//! instructions.
//!
//! The count has one writer at a time. Sending a continuation and counting
//! it are one decision under this agent's lock, so the count a continuation
//! states is the count committed, and only a continuation actually sent is
//! counted. A stale count is forgotten under the same lock, only on
//! evidence newer than it, and never by waiting on a delivery.
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

use std::path::{Path, PathBuf};

/// The delivery at which the hook's message escalates.
pub(crate) const ESCALATE_AT: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Repeat {
    pub(crate) session: String,
    /// The delivered items' typed identities, sorted.
    pub(crate) items: Vec<String>,
    pub(crate) count: u32,
    /// The newest turn in the agent's record at that delivery, where the
    /// adapter names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn: Option<String>,
    /// The furthest native record position a counted delivery came from:
    /// a Stop from at or before it has already been counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) high: Option<u64>,
    /// When it was recorded, in nanoseconds since the epoch: older evidence
    /// cannot judge it stale.
    #[serde(default)]
    pub(crate) at_ns: u128,
}

/// What one delivery is, from the hook's own native evidence.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Delivery {
    pub(crate) session: String,
    pub(crate) items: Vec<String>,
    pub(crate) turn: Option<String>,
    /// Where in the agent's own record this Stop came from, taken when the
    /// hook was invoked: the offset of the newest turn-boundary record.
    /// `None` where the adapter keeps no such record: each delivery then
    /// counts.
    pub(crate) position: Option<u64>,
    /// The newest turn is positively a person's.
    pub(crate) human_turn: bool,
}

pub(crate) fn path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("repeat.json")
}

pub(crate) fn load(agent_dir: &Path) -> Option<Repeat> {
    serde_json::from_str(&std::fs::read_to_string(path(agent_dir)).ok()?).ok()
}

pub(crate) fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Hold this agent's repeat lock, for a test that needs a sender to wait.
#[cfg(test)]
pub(crate) fn hold(agent_dir: &Path) -> std::fs::File {
    lock(agent_dir, true).expect("lock")
}

/// This agent's repeat lock, held until the returned file is dropped.
/// `wait` false gives up at once when another holds it.
fn lock(agent_dir: &Path, wait: bool) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(agent_dir).ok()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(agent_dir.join("repeat.lock"))
        .ok()?;
    let how = if wait {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    // SAFETY: flock on a descriptor this function owns.
    (unsafe { libc::flock(lock.as_raw_fd(), how) } == 0).then_some(lock)
}

fn store_unlocked(agent_dir: &Path, repeat: Option<&Repeat>) -> std::io::Result<()> {
    let path = path(agent_dir);
    let Some(repeat) = repeat else {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    };
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(repeat)?)?;
    std::fs::rename(&tmp, &path)
}

/// What a delivery is, measured against the count committed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// A new delivery: this is the count it makes.
    Counted(Repeat),
    /// The same Stop, for the same items, already counted: its count.
    Duplicate(u32),
    /// Placed at or before a Stop already counted, for other items: a
    /// capture overtaken by newer deliveries. Neither counted nor given
    /// another context's count.
    Superseded,
}

fn decide(prev: Option<&Repeat>, delivery: &Delivery, at_ns: u128) -> Decision {
    let mut items = delivery.items.clone();
    items.sort();
    items.dedup();
    let session = prev.filter(|p| p.session == delivery.session);
    if let (Some(p), Some(high), Some(here)) =
        (session, session.and_then(|p| p.high), delivery.position)
        && here <= high
    {
        return if p.items == items {
            Decision::Duplicate(p.count)
        } else {
            Decision::Superseded
        };
    }
    let same =
        session.filter(|p| p.items == items && !(delivery.human_turn && p.turn != delivery.turn));
    Decision::Counted(Repeat {
        session: delivery.session.clone(),
        items,
        count: same.map_or(1, |p| p.count.saturating_add(1)),
        turn: delivery.turn.clone(),
        high: delivery.position.max(session.and_then(|p| p.high)),
        at_ns,
    })
}

/// Send one continuation and count it, as one decision under the agent's
/// lock. `send` is handed the count to state (`Some(0)` for a superseded
/// capture, which states none), or `None` if the binding was lost while
/// waiting for the lock, when it must send nothing to the agent. It must
/// write and flush; the count is committed only if that succeeded, the
/// agent was given it, and the binding still holds. Nothing is ever
/// counted ahead of a send.
///
/// Without the lock (the agent dir cannot be written) it sends uncounted.
pub(crate) fn send_counted<T>(
    agent_dir: &Path,
    delivery: &Delivery,
    bound: impl Fn() -> bool,
    send: impl FnOnce(Option<u32>) -> std::io::Result<(T, bool)>,
) -> std::io::Result<(T, bool)> {
    let Some(_held) = lock(agent_dir, true) else {
        return send(Some(0));
    };
    if !bound() {
        return send(None);
    }
    let decision = decide(load(agent_dir).as_ref(), delivery, now_ns());
    let count = match &decision {
        Decision::Counted(next) => next.count,
        Decision::Duplicate(count) => *count,
        Decision::Superseded => 0,
    };
    let sent = send(Some(count))?;
    if sent.1
        && let Decision::Counted(next) = &decision
        && bound()
    {
        let _ = store_unlocked(agent_dir, Some(next));
    }
    Ok(sent)
}

/// Forget the count if it no longer describes the agent: bound to another
/// session now, or other items owed. A block raised or answered changes the
/// items, since a block is itself one of them.
///
/// The binding is read under the lock, as it is when this runs. `owed_now`
/// was read at `as_of_ns`, so a count recorded after that is newer than the
/// evidence against it and stands. While a delivery holds the lock, this
/// gives up rather than wait; the next fold looks again.
pub(crate) fn forget_if_stale(
    agent_dir: &Path,
    bound_session: impl FnOnce() -> Option<String>,
    owed_now: &[String],
    as_of_ns: u128,
) {
    let Some(_held) = lock(agent_dir, false) else {
        return;
    };
    let Some(repeat) = load(agent_dir) else {
        return;
    };
    let rebound = bound_session().as_deref() != Some(repeat.session.as_str());
    if !rebound && repeat.at_ns > as_of_ns {
        return;
    }
    let mut owed: Vec<&String> = owed_now.iter().collect();
    owed.sort();
    owed.dedup();
    let stale = rebound
        || owed.len() != repeat.items.len()
        || owed.iter().zip(&repeat.items).any(|(a, b)| *a != b);
    if stale {
        let _ = store_unlocked(agent_dir, None);
    }
}

/// The paragraph the hook adds once the same items have been delivered
/// [`ESCALATE_AT`] times: the repetition, the limits it does not lift,
/// and `duties`, one line per item saying what discharges it.
pub(crate) fn escalation(count: u32, duties: &[String]) -> Option<String> {
    (count >= ESCALATE_AT).then(|| {
        let mut text = format!(
            "\n\nThese items have been delivered to you {count} times unchanged. Ending your \
             turn does not discharge them: they stay owed and will be delivered again. Follow \
             the user's latest instruction, any explicit pause, and the actual reason given for \
             any rejected action; repeated delivery grants no new authority."
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

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn delivery(items: &[&str], position: Option<u64>) -> Delivery {
        Delivery {
            session: "s".into(),
            items: ids(items),
            position,
            ..Default::default()
        }
    }

    /// Send `d`: what it stated, given whether the send succeeds.
    fn send(dir: &Path, d: &Delivery, ok: bool) -> Option<u32> {
        send_counted(
            dir,
            d,
            || true,
            |count| {
                if ok {
                    Ok((count.expect("bound"), true))
                } else {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                }
            },
        )
        .ok()
        .map(|(count, _)| count)
    }

    fn committed(dir: &Path) -> Option<u32> {
        load(dir).map(|r| r.count)
    }

    #[test]
    fn the_same_set_in_any_order_counts_up_and_anything_else_restarts() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            send(dir.path(), &delivery(&["b", "a"], Some(1)), true),
            Some(1)
        );
        assert_eq!(
            send(dir.path(), &delivery(&["a", "b", "a"], Some(2)), true),
            Some(2)
        );
        assert_eq!(send(dir.path(), &delivery(&["a"], Some(3)), true), Some(1));
        let other_session = Delivery {
            session: "t".into(),
            ..delivery(&["a"], Some(1))
        };
        assert_eq!(send(dir.path(), &other_session, true), Some(1));
    }

    /// Only sent continuations count, in the order they were sent: a failure
    /// before or after a success, two failures, or a capture never sent at
    /// all leave the count where the successes put it.
    #[test]
    fn only_continuations_actually_sent_are_counted() {
        for order in [[true, false], [false, true], [false, false]] {
            let dir = tempfile::tempdir().unwrap();
            send(dir.path(), &delivery(&["a"], Some(1)), true);
            send(dir.path(), &delivery(&["a"], Some(2)), true);
            let stated: Vec<_> = order
                .iter()
                .zip([3, 4])
                .map(|(ok, pos)| send(dir.path(), &delivery(&["a"], Some(pos)), *ok))
                .collect();
            let successes = order.iter().filter(|ok| **ok).count() as u32;
            assert_eq!(committed(dir.path()), Some(2 + successes), "{order:?}");
            assert_eq!(
                stated.iter().flatten().max().copied(),
                (successes > 0).then_some(3)
            );
        }
    }

    /// A Stop from no further in the record than one already counted is not
    /// counted again, however late it comes; its send succeeding or failing
    /// changes nothing. Without a record, each counts.
    #[test]
    fn a_stop_already_counted_is_not_counted_again() {
        let dir = tempfile::tempdir().unwrap();
        send(dir.path(), &delivery(&["a"], Some(10)), true);
        send(dir.path(), &delivery(&["a"], Some(20)), true);
        for _ in 0..40 {
            assert_eq!(send(dir.path(), &delivery(&["a"], Some(10)), true), Some(2));
        }
        assert_eq!(send(dir.path(), &delivery(&["a"], Some(20)), false), None);
        assert_eq!(committed(dir.path()), Some(2));
        assert_eq!(send(dir.path(), &delivery(&["a"], None), true), Some(3));
        assert_eq!(send(dir.path(), &delivery(&["a"], None), true), Some(4));
    }

    /// Overlapping sends are serialised: each states, and commits, its own
    /// count. (With record positions, one placed earlier than a send already
    /// counted is a replay; see the test above.)
    #[test]
    fn overlapping_sends_each_state_and_store_their_own_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut stated: Vec<u32> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let dir = dir.path();
                    scope.spawn(move || send(dir, &delivery(&["a"], None), true).unwrap())
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        stated.sort();
        assert_eq!(stated, (1..=8).collect::<Vec<_>>());
        assert_eq!(committed(dir.path()), Some(8));
    }

    /// The monitor judges a count only by evidence at least as new as it,
    /// reads the binding as it is now, and never waits on a delivery.
    #[test]
    fn a_count_is_forgotten_only_on_current_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let bound = || Some("s".to_string());
        let read_before = now_ns();
        send(dir.path(), &delivery(&["b"], Some(1)), true);
        forget_if_stale(dir.path(), bound, &ids(&["a"]), read_before);
        assert!(
            load(dir.path()).is_some(),
            "the owed set was read before it"
        );
        forget_if_stale(dir.path(), bound, &ids(&["b"]), now_ns());
        assert!(load(dir.path()).is_some(), "still the same items");
        forget_if_stale(dir.path(), || Some("t".into()), &ids(&["b"]), read_before);
        assert!(
            load(dir.path()).is_none(),
            "bound elsewhere now, whatever was read"
        );

        send(dir.path(), &delivery(&["b"], Some(2)), true);
        let held = lock(dir.path(), true).unwrap();
        forget_if_stale(dir.path(), || None, &ids(&["x"]), now_ns());
        drop(held);
        assert!(
            load(dir.path()).is_some(),
            "a held lock is skipped, not waited on"
        );
    }

    /// Only a turn the record positively calls a person's resets the
    /// count, and only once it is newer than the last delivery's.
    #[test]
    fn only_a_known_persons_newer_turn_resets() {
        let dir = tempfile::tempdir().unwrap();
        let turn = |turn: &str, human: bool, pos: u64| Delivery {
            turn: Some(turn.into()),
            human_turn: human,
            ..delivery(&["a"], Some(pos))
        };
        assert_eq!(send(dir.path(), &turn("p1", false, 1), true), Some(1));
        assert_eq!(send(dir.path(), &turn("p2", false, 2), true), Some(2));
        assert_eq!(send(dir.path(), &turn("p3", true, 3), true), Some(1));
        assert_eq!(send(dir.path(), &turn("p4", false, 4), true), Some(2));
    }

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

    /// A capture overtaken by newer deliveries of other items is sent
    /// uncounted, stating no count, and leaves theirs alone; a duplicate of
    /// the same items states their count. Of two captures sent out of
    /// order, the earlier-placed one is overtaken.
    #[test]
    fn an_overtaken_capture_states_no_count_and_changes_none() {
        let dir = tempfile::tempdir().unwrap();
        send(dir.path(), &delivery(&["a"], Some(1)), true);
        for pos in 3..=6 {
            send(dir.path(), &delivery(&["b"], Some(pos)), true);
        }
        assert_eq!(send(dir.path(), &delivery(&["a"], Some(2)), true), Some(0));
        let b = load(dir.path()).unwrap();
        assert_eq!((b.items.clone(), b.count), (ids(&["b"]), 4));
        assert_eq!(
            send(dir.path(), &delivery(&["a"], Some(6)), true),
            Some(0),
            "other items at the same place"
        );
        assert_eq!(
            send(dir.path(), &delivery(&["b"], Some(5)), true),
            Some(4),
            "a duplicate"
        );
        assert_eq!(load(dir.path()).unwrap().count, 4);
    }

    /// A send that waited for the lock while the session was rebound sends
    /// nothing to the agent and counts nothing.
    #[test]
    fn a_rebind_while_waiting_for_the_lock_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        send(dir.path(), &delivery(&["a"], Some(1)), true);
        let handed = send_counted(
            dir.path(),
            &delivery(&["a"], Some(2)),
            || false,
            |count| Ok((count, true)),
        )
        .unwrap();
        assert_eq!(handed.0, None);
        assert_eq!(load(dir.path()).unwrap().count, 1);
    }
}
