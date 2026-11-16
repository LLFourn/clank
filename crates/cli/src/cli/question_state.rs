//! Whether a Claude agent is waiting on a question it asked the user, kept
//! where status, the TUI, the remote status and recovery can all read it
//! without a viewer.
//!
//! One reducer ([`apply`]) takes every observation of a question's
//! lifecycle, from the question hook and from the agent's own record, and
//! keeps one durable projection of the bound session's questions, its
//! [`Bound::Questions`] part of the agent's state. It is separate from the
//! native answer channel: recording a pending question never claims a
//! live remote answer request exists.

use std::path::Path;

use crate::agent_store::state::{self, Bound};

/// A question's native identity: its session, the prompt (turn) it was
/// asked in, and the tool call.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub(crate) struct Key {
    pub(crate) session: String,
    pub(crate) prompt: String,
    pub(crate) tool_use: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Question {
    pub(crate) key: Key,
    /// What the agent is asking, for one status line.
    pub(crate) header: String,
    /// The asked input's hash, as the answer channel correlates a
    /// `PermissionRequest` by it.
    pub(crate) input: String,
}

/// The projection, about the session bound to the agent. What ended is
/// kept for as long as that session is the agent's, so a late or replayed
/// hook cannot make it pending again; a new binding starts with none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) pending: Vec<Question>,
    /// Prompts known to be over, whether or not anything was asked in them.
    pub(crate) over: Vec<String>,
    /// Calls that returned, in prompts not known to be over.
    pub(crate) returned: Vec<Key>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "seen", rename_all = "snake_case")]
pub(crate) enum Observation {
    /// `PreToolUse`: the question is being asked.
    Asked {
        key: Key,
        header: String,
        input: String,
    },
    /// The matching `PostToolUse` or `PostToolUseFailure`.
    Result { key: Key },
    /// Proof that turn `prompt` of `session` ended: its Stop, or a failure
    /// the native record places in it.
    TurnOver { session: String, prompt: String },
    /// Proof that `older` prompts of `session` are behind a newer one: the
    /// native record shows them before it.
    Behind { session: String, older: Vec<String> },
}

/// [`State`] as stored under its session, whose keys name no session.
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Stored {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<StoredQuestion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    over: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    returned: Vec<Call>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Call {
    prompt: String,
    tool_use: String,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StoredQuestion {
    #[serde(flatten)]
    call: Call,
    header: String,
    input: String,
}

impl Call {
    fn of(key: &Key) -> Self {
        Call {
            prompt: key.prompt.clone(),
            tool_use: key.tool_use.clone(),
        }
    }

    fn key(self, session: &str) -> Key {
        Key {
            session: session.to_string(),
            prompt: self.prompt,
            tool_use: self.tool_use,
        }
    }
}

impl State {
    fn stored(&self) -> Stored {
        Stored {
            pending: self
                .pending
                .iter()
                .map(|q| StoredQuestion {
                    call: Call::of(&q.key),
                    header: q.header.clone(),
                    input: q.input.clone(),
                })
                .collect(),
            over: self.over.clone(),
            returned: self.returned.iter().map(Call::of).collect(),
        }
    }

    fn restored(stored: Stored, session: &str) -> State {
        State {
            pending: stored
                .pending
                .into_iter()
                .map(|q| Question {
                    key: q.call.key(session),
                    header: q.header,
                    input: q.input,
                })
                .collect(),
            over: stored.over,
            returned: stored
                .returned
                .into_iter()
                .map(|c| c.key(session))
                .collect(),
        }
    }
}

fn end_prompts(state: &mut State, prompts: &[String]) {
    state.pending.retain(|q| !prompts.contains(&q.key.prompt));
    state.returned.retain(|k| !prompts.contains(&k.prompt));
    for prompt in prompts {
        if !state.over.contains(prompt) {
            state.over.push(prompt.clone());
        }
    }
}

/// Apply one observation to the questions of `bound`. One about another
/// session is a stale producer's, and changes nothing.
pub(crate) fn apply(mut state: State, bound: &str, observation: Observation) -> State {
    let session = match &observation {
        Observation::Asked { key, .. } | Observation::Result { key } => &key.session,
        Observation::TurnOver { session, .. } | Observation::Behind { session, .. } => session,
    };
    if session != bound {
        return state;
    }
    match observation {
        Observation::Asked { key, header, input } => {
            let known = state.over.contains(&key.prompt)
                || state.returned.contains(&key)
                || state.pending.iter().any(|q| q.key == key);
            if !known {
                state.pending.push(Question { key, header, input });
            }
        }
        Observation::Result { key } => {
            state.pending.retain(|q| q.key != key);
            if !state.over.contains(&key.prompt) && !state.returned.contains(&key) {
                state.returned.push(key);
            }
        }
        Observation::TurnOver { prompt, .. } => end_prompts(&mut state, &[prompt]),
        Observation::Behind { older, .. } => end_prompts(&mut state, &older),
    }
    state
}

/// The questions of `session` while it holds the label, none otherwise. A
/// record that cannot be read is unavailable: `None`, which shows nothing,
/// holds nothing and is never taken for an answer.
pub(crate) fn load(agent_dir: &Path, session: &str) -> Option<State> {
    let state = state::State::load(agent_dir).ok()?;
    let session_id = clank_core::SessionId::parse(session).ok()?;
    let stored: Option<Stored> = state.bound(&session_id, Bound::Questions).ok()?;
    Some(
        stored
            .map(|s| State::restored(s, session))
            .unwrap_or_default(),
    )
}

/// Apply `observations` to the bound session's questions, under the
/// agent's lock, against the binding read under that lock: a producer that
/// read an older binding cannot write over the current session's state.
/// Best effort: what is not recorded is logged.
pub(crate) fn observe(agent_dir: &Path, observations: impl IntoIterator<Item = Observation>) {
    match try_observe(agent_dir, observations) {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!("question evidence was not recorded: the agent's lock is held")
        }
        Err(e) => tracing::debug!(%e, "question evidence was not recorded"),
    }
}

/// [`observe`], saying whether the evidence was recorded. Never waits for
/// the agent's lock, as the monitor's loop must not (I9): `false`, with
/// nothing recorded, while another holds it. An unreadable record is an
/// error and is left as it is.
pub(crate) fn try_observe(
    agent_dir: &Path,
    observations: impl IntoIterator<Item = Observation>,
) -> anyhow::Result<bool> {
    let permit = crate::upgrade::writing()?;
    let done = state::try_transact(agent_dir, &permit, |txn| {
        let Some(binding) = txn
            .binding()?
            .filter(|s| s.tool == clank_core::vocab::Tool::Claude)
        else {
            return Ok(());
        };
        let session = binding.id.as_str();
        let stored: Option<Stored> = txn.bound(&binding.id, Bound::Questions)?;
        let before = stored
            .map(|s| State::restored(s, session))
            .unwrap_or_default();
        let after = observations
            .into_iter()
            .fold(before.clone(), |s, o| apply(s, session, o));
        if after != before {
            txn.patch(&binding.id, Bound::Questions, Some(&after.stored()))?;
        }
        Ok(())
    })?;
    Ok(done.is_some())
}

/// The question the agent is waiting on in `session`, if one is pending.
pub(crate) fn asking(agent_dir: &Path, session: &str) -> Option<Question> {
    load(agent_dir, session)?.pending.into_iter().next_back()
}

fn prompt_id(line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("promptId")?
        .as_str()
        .map(str::to_string)
}

/// The prompts of `session` the native record shows before `newest`: the
/// ones a newer turn has left behind. Read from the last stretch only; a
/// prompt not in it is not known to be behind.
pub(crate) fn prompts_behind(transcript: &Path, newest: &str) -> Vec<String> {
    const WINDOW: u64 = 256 * 1024;
    let Ok((_, lines)) = crate::cli::web::transcript::Tail::open(transcript, WINDOW) else {
        return Vec::new();
    };
    let mut seen: Vec<String> = Vec::new();
    for id in lines.iter().filter_map(|l| prompt_id(&l.text)) {
        if id == newest {
            return seen;
        }
        if !seen.contains(&id) {
            seen.push(id);
        }
    }
    Vec::new()
}

/// The calls a native record line returns, under its prompt: a Claude
/// user record's `tool_result` blocks.
pub(crate) fn results_in(line: &str, session: &str) -> Vec<Key> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let Some(prompt) = v.get("promptId").and_then(|p| p.as_str()) else {
        return Vec::new();
    };
    let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id").and_then(|id| id.as_str()))
        .map(|id| Key {
            session: session.to_string(),
            prompt: prompt.to_string(),
            tool_use: id.to_string(),
        })
        .collect()
}

/// What the native record proves ended the call `key`: its result (a
/// cancelled question has one, and no result hook), or a newer prompt
/// after the call's own. Reads back from the end in widening stretches
/// until the stretch holds the start of the call's prompt, so an old
/// call costs one long read; a call the record does not show yet proves
/// nothing (hooks run before the record publishes their prompt).
pub(crate) fn recorded_end(transcript: &Path, key: &Key) -> Option<Observation> {
    let len = std::fs::metadata(transcript).ok()?.len();
    let mut window: u64 = 256 * 1024;
    loop {
        let (_, lines) = crate::cli::web::transcript::Tail::open(transcript, window).ok()?;
        let (mut began, mut asked_in, mut after) = (false, false, false);
        let mut before = false;
        for line in &lines {
            if results_in(&line.text, &key.session).contains(key) {
                return Some(Observation::Result { key: key.clone() });
            }
            match prompt_id(&line.text) {
                Some(id) if id == key.prompt => {
                    began |= !asked_in && before;
                    asked_in = true;
                }
                Some(_) if asked_in => after = true,
                Some(_) => before = true,
                None => {}
            }
        }
        if after {
            return Some(Observation::Behind {
                session: key.session.clone(),
                older: vec![key.prompt.clone()],
            });
        }
        if began || window >= len {
            return None;
        }
        window = window.saturating_mul(4);
    }
}

/// The hash a question's input is correlated by: its normalized tool input.
pub(crate) fn input_hash(input: &serde_json::Value) -> String {
    let mut value = input.clone();
    if let Some(m) = value.as_object_mut() {
        m.remove("answers");
        m.remove("annotations");
    }
    blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(session: &str, prompt: &str, tool: &str) -> Key {
        Key {
            session: session.into(),
            prompt: prompt.into(),
            tool_use: tool.into(),
        }
    }

    fn asked(k: &Key) -> Observation {
        Observation::Asked {
            key: k.clone(),
            header: "Life monitor".into(),
            input: "h".into(),
        }
    }

    fn result_of(k: &Key) -> Observation {
        Observation::Result { key: k.clone() }
    }

    fn turn_over(session: &str, prompt: &str) -> Observation {
        Observation::TurnOver {
            session: session.into(),
            prompt: prompt.into(),
        }
    }

    fn behind(session: &str, older: &[&str]) -> Observation {
        Observation::Behind {
            session: session.into(),
            older: older.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// Observations of an agent bound to session `s` throughout.
    fn run(obs: impl IntoIterator<Item = Observation>) -> State {
        obs.into_iter()
            .fold(State::default(), |s, o| apply(s, "s", o))
    }

    fn pending(state: &State) -> Vec<Key> {
        state.pending.iter().map(|q| q.key.clone()).collect()
    }

    #[test]
    fn a_question_is_pending_until_its_own_result() {
        let q = key("s", "p1", "t1");
        let state = run([asked(&q), result_of(&key("s", "p1", "t2"))]);
        assert_eq!(pending(&state), vec![q.clone()], "another call's result");
        assert!(pending(&apply(state, "s", result_of(&q))).is_empty());
    }

    /// Whatever order a question's hooks and its endings arrive in, and
    /// however often they are replayed, an ended question stays ended.
    #[test]
    fn an_ended_question_is_never_revived() {
        let q = key("s", "p1", "t1");
        for (why, obs) in [
            (
                "replayed after its result",
                vec![asked(&q), result_of(&q), asked(&q)],
            ),
            ("its result before it", vec![result_of(&q), asked(&q)]),
            (
                "its turn's Stop before it",
                vec![turn_over("s", "p1"), asked(&q)],
            ),
            (
                "a newer prompt before it",
                vec![behind("s", &["p1"]), asked(&q)],
            ),
            (
                "replayed after many later questions",
                std::iter::once(asked(&q))
                    .chain(std::iter::once(result_of(&q)))
                    .chain((0..100).flat_map(|i| {
                        let later = key("s", "p1", &format!("later{i}"));
                        [asked(&later), result_of(&later)]
                    }))
                    .chain(std::iter::once(asked(&q)))
                    .collect(),
            ),
        ] {
            assert!(run(obs).pending.is_empty(), "{why}");
        }
        assert_eq!(run([asked(&q), asked(&q)]).pending.len(), 1, "duplicates");
    }

    /// A turn's end ends only the questions of that turn, and what a
    /// prompt's end subsumes is not also kept call by call.
    #[test]
    fn a_turn_ends_only_its_own_questions() {
        let q = key("s", "p1", "t1");
        let answered = key("s", "p1", "t0");
        let state = run([
            asked(&q),
            result_of(&answered),
            turn_over("s", "p0"),
            behind("s", &["p2"]),
        ]);
        assert_eq!(pending(&state), vec![q], "another turn's end");
        let state = apply(state, "s", turn_over("s", "p1"));
        assert!(state.pending.is_empty());
        assert!(state.returned.is_empty());
        assert_eq!(state.over, vec!["p0", "p2", "p1"]);
    }

    /// A producer that read an older binding cannot touch the current
    /// session's state.
    #[test]
    fn only_the_bound_session_is_observed() {
        let old = key("old", "p1", "t1");
        let q = key("s", "p1", "t1");
        let state = run([
            asked(&old),
            turn_over("old", "p1"),
            asked(&q),
            result_of(&key("old", "p1", "t1")),
        ]);
        assert_eq!(pending(&state), vec![q.clone()]);
        assert!(state.over.is_empty());
        assert!(
            apply(State::default(), "new", asked(&q)).pending.is_empty(),
            "a stale hook"
        );
    }

    fn bind(dir: &Path, session: &str) {
        crate::agent_store::state::tests::bind(dir, clank_core::Tool::Claude, session);
    }

    /// The record survives on disk and follows the binding read under its
    /// lock; one that cannot be read shows and holds nothing, and is not
    /// overwritten.
    #[test]
    fn the_record_is_durable_and_follows_the_binding() {
        let dir = tempfile::tempdir().unwrap();
        let q = key("session-s", "p1", "t1");
        observe(dir.path(), [asked(&q)]);
        assert_eq!(asking(dir.path(), "session-s"), None, "an unbound agent");
        bind(dir.path(), "session-s");
        observe(dir.path(), [asked(&q)]);
        assert_eq!(
            asking(dir.path(), "session-s").map(|q| q.header),
            Some("Life monitor".into())
        );
        assert_eq!(asking(dir.path(), "session-t"), None, "another session");
        bind(dir.path(), "session-t");
        observe(dir.path(), [asked(&q)]);
        assert_eq!(asking(dir.path(), "session-s"), None, "a stale producer");
        bind(dir.path(), "session-s");
        observe(dir.path(), [asked(&q)]);
        assert!(asking(dir.path(), "session-s").is_some());
        let mut raw = crate::agent_store::state::tests::raw(dir.path());
        raw["bound"]["questions"] = "not questions".into();
        let unreadable = raw.to_string();
        std::fs::write(crate::agent_store::state::path(dir.path()), &unreadable).unwrap();
        assert_eq!(asking(dir.path(), "session-s"), None);
        assert!(try_observe(dir.path(), [result_of(&q)]).is_err());
        assert_eq!(
            std::fs::read_to_string(crate::agent_store::state::path(dir.path())).unwrap(),
            unreadable
        );
    }

    /// Recorded questions name their session once, in the binding.
    #[test]
    fn stored_questions_name_no_session_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        bind(dir.path(), "session-s");
        let (q, done) = (key("session-s", "p1", "t1"), key("session-s", "p1", "t0"));
        observe(dir.path(), [asked(&q), result_of(&done)]);
        let stored = std::fs::read_to_string(crate::agent_store::state::path(dir.path())).unwrap();
        assert_eq!(stored.matches("session-s").count(), 1, "{stored}");
        let state = load(dir.path(), "session-s").unwrap();
        assert_eq!(pending(&state), vec![q]);
        assert_eq!(state.returned, vec![done]);
    }

    fn transcript(dir: &Path, prompts: &[&str], filler: usize) -> std::path::PathBuf {
        let t = dir.join("t.jsonl");
        let pad = "x".repeat(filler);
        let body: String = prompts
            .iter()
            .map(|id| format!("{{\"type\":\"user\",\"promptId\":\"{id}\",\"pad\":\"{pad}\"}}\n"))
            .collect();
        std::fs::write(&t, body).unwrap();
        t
    }

    #[test]
    fn prompts_behind_a_newer_one_are_read_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let t = transcript(dir.path(), &["p1", "p1", "p2", "p3"], 0);
        assert_eq!(
            prompts_behind(&t, "p3"),
            vec!["p1".to_string(), "p2".into()]
        );
        assert_eq!(prompts_behind(&t, "p1"), Vec::<String>::new());
        assert_eq!(prompts_behind(&t, "nowhere"), Vec::<String>::new());
    }

    /// However far back a call is, the record's own result for it, or a
    /// newer prompt after its own, ends it; a call not yet published, or
    /// still waiting at the end of the record, is not ended.
    #[test]
    fn the_record_ends_a_call_however_far_back_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("t.jsonl");
        let pad = "x".repeat(64 * 1024);
        let user = |id: &str| format!(r#"{{"type":"user","promptId":"{id}","pad":"{pad}"}}"#);
        let result = |id: &str, call: &str| {
            format!(
                r#"{{"type":"user","promptId":"{id}","message":{{"content":[{{"type":"tool_result","tool_use_id":"{call}","is_error":true}}]}}}}"#
            )
        };
        let mut lines = vec![user("old"), result("old", "answered")];
        lines.extend(std::iter::repeat_n(user("new"), 40));
        lines.push(result("new", "cancelled"));
        lines.extend([user("long"), result("long", "early")]);
        lines.extend(std::iter::repeat_n(user("long"), 10));
        std::fs::write(&t, lines.join("\n") + "\n").unwrap();
        assert!(std::fs::metadata(&t).unwrap().len() > 2 * 1024 * 1024);
        assert!(prompts_behind(&t, "long").is_empty(), "beyond the stretch");
        let k = |prompt: &str, call: &str| key("s", prompt, call);
        assert_eq!(
            recorded_end(&t, &k("old", "answered")),
            Some(result_of(&k("old", "answered")))
        );
        assert_eq!(
            recorded_end(&t, &k("old", "waiting")),
            Some(behind("s", &["old"]))
        );
        assert_eq!(
            recorded_end(&t, &k("new", "cancelled")),
            Some(result_of(&k("new", "cancelled")))
        );
        assert_eq!(
            recorded_end(&t, &k("long", "early")),
            Some(result_of(&k("long", "early"))),
            "before the newest prompt's last stretch"
        );
        assert_eq!(recorded_end(&t, &k("long", "waiting")), None, "the newest");
        assert_eq!(recorded_end(&t, &k("unpublished", "t")), None);
    }
}
