//! A harness's own record that it accepted a prompt: Claude's transcript
//! or Codex's rollout, read only past a mark taken before clank types.
//!
//! Claude writes a `user` record when it takes a prompt idle and a
//! `queue-operation` enqueue when it takes one busy, each holding the whole
//! text; Codex's rollout records a `user_message`. Nothing else here says a
//! prompt arrived.

use super::pane_input::Delivered;
use clank_core::Tool;
use clank_core::ids::AgentLabel;
use std::path::PathBuf;

/// The session bound to an agent now, and where its record is.
pub(crate) trait Binding {
    /// What selects the bound session's record, equal across reads only
    /// while the binding is unchanged; `Err` when there is none.
    fn key(&self) -> Result<String, String>;
    /// [`Binding::key`] and the record's path, `None` while that record is
    /// still to be found; `Err` when there is no binding or no record clank
    /// can read.
    fn record(&self) -> Result<(String, Option<PathBuf>), String>;
}

/// An agent's stored binding.
#[derive(Debug, Clone)]
pub(crate) struct Stored {
    pub(crate) repo: PathBuf,
    pub(crate) label: AgentLabel,
}

impl Stored {
    fn bound(&self) -> Result<clank_core::agent_config::Session, String> {
        crate::agent_store::load_agent_config(&self.repo, &self.label)
            .map_err(|e| format!("its binding could not be read: {e:#}"))?
            .and_then(|c| c.session)
            .ok_or_else(|| "it has no bound session".to_string())
    }
}

/// The id alone is not enough: a binding can move to another transcript
/// under the same id (`agent_store::record_transcript`), and the old file
/// must not confirm a prompt the new one never took.
fn key(session: &clank_core::agent_config::Session) -> String {
    format!("{} {} {:?}", session.tool, session.id, session.transcript)
}

impl Binding for Stored {
    fn key(&self) -> Result<String, String> {
        Ok(key(&self.bound()?))
    }

    fn record(&self) -> Result<(String, Option<PathBuf>), String> {
        use super::recovery::Source;
        let session = self.bound()?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let agent_dir = crate::agent_store::agents_root(&self.repo).join(self.label.as_str());
        let id = key(&session);
        match super::recovery::record_source(&agent_dir, &session, &self.repo, &home) {
            Some(Source::At(path)) => Ok((id, Some(path))),
            Some(Source::Find { .. }) => Ok((id, None)),
            None => Err(format!("{} keeps no record clank can read", session.tool)),
        }
    }
}

/// What an attempt reads its confirmation from.
pub(crate) trait Receipts {
    /// Mark where the record ends now; only what is appended past it is
    /// the attempt's.
    async fn mark(&mut self) -> Result<(), String>;
    /// The whole lines appended since the last read, or why none can be
    /// attributed to the attempt.
    async fn read(&mut self) -> Result<Vec<String>, String>;
}

/// `binding`'s record, read on the blocking pool so a slow filesystem
/// holds no executor thread; the caller bounds each read.
pub(crate) struct Offloaded<B> {
    pub(crate) binding: B,
    tail: Option<Tail>,
}

impl<B> Offloaded<B> {
    pub(crate) fn new(binding: B) -> Self {
        Offloaded {
            binding,
            tail: None,
        }
    }
}

impl<B: Binding + Clone + Send + 'static> Receipts for Offloaded<B> {
    async fn mark(&mut self) -> Result<(), String> {
        let binding = self.binding.clone();
        self.tail = Some(offload(move || Tail::mark(&binding)).await??);
        Ok(())
    }

    async fn read(&mut self) -> Result<Vec<String>, String> {
        let mut tail = self.tail.take().ok_or("its record was not marked")?;
        let binding = self.binding.clone();
        let (tail, lines) = offload(move || {
            let lines = tail.read(&binding);
            (tail, lines)
        })
        .await?;
        self.tail = Some(tail);
        lines
    }
}

async fn offload<R: Send + 'static>(
    work: impl FnOnce() -> R + Send + 'static,
) -> Result<R, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("reading its record failed: {e}"))
}

/// A bound session's record, followed from where it ended when an attempt
/// began, under that binding, in that file.
#[derive(Debug)]
pub(crate) struct Tail {
    session: String,
    /// Found once: a record not yet found is looked for on each read.
    path: Option<PathBuf>,
    /// The file by device and inode; `None` while it did not exist.
    file: Option<(u64, u64)>,
    /// How far it has been read.
    at: u64,
    /// The bytes after the last whole line read.
    partial: Vec<u8>,
    /// The mark fell inside a record still being written, which began
    /// before the attempt: the first line completed is not the attempt's.
    inside: bool,
}

impl Tail {
    /// Where `binding`'s record ends now.
    pub(crate) fn mark(binding: &impl Binding) -> Result<Tail, String> {
        use std::os::unix::fs::{FileExt, MetadataExt};
        let (session, path) = binding.record()?;
        let opened = match path.as_deref().map(std::fs::File::open) {
            None => None,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Some(Err(e)) => return Err(format!("its record could not be read: {e}")),
            Some(Ok(file)) => Some(file),
        };
        let (file, at, inside) = match opened {
            None => (None, 0, false),
            Some(file) => {
                let meta = file
                    .metadata()
                    .map_err(|e| format!("its record could not be read: {e}"))?;
                let mut last = [0u8];
                let inside = meta.len() > 0
                    && file.read_at(&mut last, meta.len() - 1).is_ok()
                    && last[0] != b'\n';
                (Some((meta.dev(), meta.ino())), meta.len(), inside)
            }
        };
        Ok(Tail {
            session,
            path,
            file,
            at,
            partial: Vec::new(),
            inside,
        })
    }

    /// The whole lines appended since the last read, or why none can be
    /// attributed to the attempt: the agent was rebound, or its record
    /// shrank or was replaced.
    pub(crate) fn read(&mut self, binding: &impl Binding) -> Result<Vec<String>, String> {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let rebound = || "the agent's binding changed during the attempt".to_string();
        let path = match &self.path {
            Some(path) => {
                if binding.key()? != self.session {
                    return Err(rebound());
                }
                path.clone()
            }
            None => {
                let (session, path) = binding.record()?;
                if session != self.session {
                    return Err(rebound());
                }
                let Some(path) = path else {
                    return Ok(Vec::new());
                };
                self.path = Some(path.clone());
                path
            }
        };
        let mut file = match std::fs::File::open(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.file.is_none() => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(format!("its record could not be read: {e}")),
            Ok(file) => file,
        };
        let meta = file
            .metadata()
            .map_err(|e| format!("its record could not be read: {e}"))?;
        match self.file {
            None => self.file = Some((meta.dev(), meta.ino())),
            Some(at) if at != (meta.dev(), meta.ino()) => {
                return Err("its record was replaced during the attempt".into());
            }
            Some(_) if meta.len() < self.at => {
                return Err("its record shrank during the attempt".into());
            }
            Some(_) => {}
        }
        let mut fresh = Vec::new();
        file.seek(SeekFrom::Start(self.at))
            .and_then(|_| file.read_to_end(&mut fresh))
            .map_err(|e| format!("its record could not be read: {e}"))?;
        self.at += fresh.len() as u64;
        self.partial.extend_from_slice(&fresh);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return Ok(Vec::new());
        };
        let rest = self.partial.split_off(end + 1);
        let whole = std::mem::replace(&mut self.partial, rest);
        let mut lines: Vec<String> = whole[..end]
            .split(|b| *b == b'\n')
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();
        if std::mem::take(&mut self.inside) {
            lines.remove(0);
        }
        Ok(lines)
    }
}

/// The prompt text a record line says `tool` accepted from its input, or
/// `None` for any other line: a tool result, hook feedback (`isMeta`), a
/// compaction summary, a task notification.
pub(crate) fn input(tool: Tool, line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = match tool {
        Tool::Claude => claude_input(&v)?,
        Tool::Codex => {
            if v.get("type")?.as_str()? != "event_msg"
                || v.pointer("/payload/type")?.as_str()? != "user_message"
            {
                return None;
            }
            v.pointer("/payload/message")?.as_str()?.to_string()
        }
        Tool::Grok | Tool::OpenCode => return None,
    };
    (!text.starts_with("<task-notification>")).then_some(text)
}

fn claude_input(v: &serde_json::Value) -> Option<String> {
    match v.get("type")?.as_str()? {
        "queue-operation" if v.get("operation")?.as_str()? == "enqueue" => {
            Some(v.get("content")?.as_str()?.to_string())
        }
        "user" => {
            let flag = |k: &str| v.get(k).and_then(|b| b.as_bool()) == Some(true);
            if flag("isMeta") || super::recovery::claude_compact_summary(v) {
                return None;
            }
            if v.pointer("/origin/kind")
                .is_some_and(|k| k.as_str() != Some("human"))
            {
                return None;
            }
            match v.pointer("/message/content")? {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(blocks) => {
                    let mut text = Vec::new();
                    for b in blocks {
                        match b.get("type")?.as_str()? {
                            "text" => text.push(b.get("text")?.as_str()?),
                            "tool_result" => return None,
                            _ => {}
                        }
                    }
                    Some(text.join("\n"))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether accepted `input` is `payload`: the whole of it, or a non-empty
/// proper suffix of it, which only a write of `payload` that lost its start
/// can produce. Anything else is someone else's prompt.
pub(crate) fn attribute(payload: &str, input: &str) -> Option<Delivered> {
    let got = unwrap_pasted(input);
    let (got, sent) = (got.trim(), payload.trim());
    if got == sent {
        Some(Delivered::Whole)
    } else if !got.is_empty() && got.len() < sent.len() && sent.ends_with(got) {
        Some(Delivered::Altered)
    } else {
        None
    }
}

/// `text` with Claude's paste wrapping taken off the way Claude takes it
/// off (2.1.308): each `<pasted_content id="hhhh">` block, with up to two
/// line breaks either side of it, becomes its body.
fn unwrap_pasted(text: &str) -> String {
    const OPEN: &str = "<pasted_content id=\"";
    let bytes = text.as_bytes();
    let mut out = String::new();
    let (mut kept, mut from) = (0, 0);
    while let Some(at) = text[from..].find(OPEN).map(|i| i + from) {
        let id_at = at + OPEN.len();
        let id = text.get(id_at..id_at + 4).filter(|id| {
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        let Some(id) = id.filter(|_| text[id_at + 4..].starts_with("\">\n")) else {
            from = id_at;
            continue;
        };
        let body = id_at + 4 + 3;
        let close = format!("\n</pasted_content id=\"{id}\">");
        let Some(end) = text[body - 1..].find(&close).map(|i| i + body - 1) else {
            break;
        };
        let mut start = at;
        for _ in 0..2 {
            if start > kept && bytes[start - 1] == b'\n' {
                start -= 1;
            }
        }
        out.push_str(&text[kept..start]);
        out.push_str(text.get(body..end).unwrap_or_default());
        kept = end + close.len();
        for _ in 0..2 {
            if bytes.get(kept) == Some(&b'\n') {
                kept += 1;
            }
        }
        from = kept;
    }
    out.push_str(&text[kept..]);
    out
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// Claude's record of a prompt taken idle.
    pub(crate) fn claude_user(text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "origin": {"kind": "human"},
            "promptId": "p",
            "message": {"role": "user", "content": text},
        })
        .to_string()
    }

    /// Claude's record of a prompt taken while busy.
    pub(crate) fn claude_enqueue(text: &str) -> String {
        serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": text,
        })
        .to_string()
    }

    /// Codex's record of a prompt.
    pub(crate) fn codex_user(text: &str) -> String {
        serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "user_message", "message": text},
        })
        .to_string()
    }

    /// `text` as Claude records a paste of it: wrapped in its tags, the
    /// way `<pasted_content>` appears in a 2.1.308 transcript.
    pub(crate) fn pasted(id: &str, text: &str) -> String {
        format!("<pasted_content id=\"{id}\">\n{text}\n</pasted_content id=\"{id}\">\n")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::path::Path;

    /// A binding that says what it is set to, counting how often its
    /// record is looked for.
    struct Fixed {
        bound: RefCell<(String, Option<PathBuf>)>,
        looked: Cell<usize>,
    }

    impl Binding for Fixed {
        fn key(&self) -> Result<String, String> {
            Ok(self.bound.borrow().0.clone())
        }
        fn record(&self) -> Result<(String, Option<PathBuf>), String> {
            self.looked.set(self.looked.get() + 1);
            Ok(self.bound.borrow().clone())
        }
    }

    fn bound(path: Option<&Path>) -> Fixed {
        Fixed {
            bound: RefCell::new(("s1".into(), path.map(Path::to_path_buf))),
            looked: Cell::new(0),
        }
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    /// Only whole lines past the mark are read, each once: not a record
    /// before it, not the rest of one being written when it was taken, not
    /// a line still being written now.
    #[test]
    fn only_new_whole_lines_past_the_mark_are_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "before\nhalf of a rec");
        let binding = bound(Some(&path));
        let mut tail = Tail::mark(&binding).unwrap();
        append(&path, "ord\nafter\npartial");
        assert_eq!(tail.read(&binding).unwrap(), ["after"]);
        assert!(tail.read(&binding).unwrap().is_empty());
        append(&path, " line\nnext\n");
        assert_eq!(tail.read(&binding).unwrap(), ["partial line", "next"]);
    }

    /// A record not yet written when the mark was taken is read whole once
    /// it appears; one not yet found is looked for until it is, then not
    /// again.
    #[test]
    fn a_record_appearing_after_the_mark_is_read_from_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let binding = bound(Some(&path));
        let mut tail = Tail::mark(&binding).unwrap();
        assert!(tail.read(&binding).unwrap().is_empty());
        append(&path, "first\n");
        assert_eq!(tail.read(&binding).unwrap(), ["first"]);

        let found = bound(None);
        let mut tail = Tail::mark(&found).unwrap();
        assert!(tail.read(&found).unwrap().is_empty());
        let rollout = dir.path().join("rollout.jsonl");
        append(&rollout, "one\n");
        found.bound.borrow_mut().1 = Some(rollout.clone());
        assert_eq!(tail.read(&found).unwrap(), ["one"]);
        let looked = found.looked.get();
        append(&rollout, "two\n");
        assert_eq!(tail.read(&found).unwrap(), ["two"]);
        assert_eq!(found.looked.get(), looked, "found once");
    }

    /// A record that shrank or was replaced, or a binding that changed:
    /// nothing past the mark can be attributed.
    #[test]
    fn a_shrunk_replaced_or_rebound_record_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "one\ntwo\n");
        let binding = bound(Some(&path));
        let mut tail = Tail::mark(&binding).unwrap();
        std::fs::write(&path, "one\n").unwrap();
        assert!(tail.read(&binding).unwrap_err().contains("shrank"));

        append(&path, "two\n");
        let mut tail = Tail::mark(&binding).unwrap();
        let other = dir.path().join("other");
        std::fs::write(&other, "one\ntwo\nthree\n").unwrap();
        std::fs::rename(&other, &path).unwrap();
        assert!(tail.read(&binding).unwrap_err().contains("replaced"));

        let mut tail = Tail::mark(&binding).unwrap();
        binding.bound.borrow_mut().0 = "s2".into();
        assert!(tail.read(&binding).unwrap_err().contains("binding changed"));
    }

    /// An agent's stored binding moved to another transcript under the same
    /// session id: a prompt then appended to the old file confirms nothing.
    #[test]
    fn a_stored_binding_moved_to_another_transcript_is_lost() {
        use crate::cli::teams_config::{AgentDescription, RosterRole};
        use clank_core::ids::SessionId;
        let repo = tempfile::tempdir().unwrap();
        let label = AgentLabel::parse("claude").unwrap();
        crate::cli::agent::add_repo_roster_agent(
            repo.path(),
            &label,
            AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
            RosterRole::Master,
        )
        .unwrap();
        let session = SessionId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        crate::agent_store::bind_session_to_agent(repo.path(), &label, Tool::Claude, &session)
            .unwrap();
        let old = repo.path().join("old.jsonl");
        append(&old, "before\n");
        crate::agent_store::record_transcript(
            repo.path(),
            &label,
            &session,
            &old.to_string_lossy(),
        )
        .unwrap();
        let stored = Stored {
            repo: repo.path().to_path_buf(),
            label: label.clone(),
        };
        let mut tail = Tail::mark(&stored).unwrap();
        let new = repo.path().join("new.jsonl");
        crate::agent_store::record_transcript(
            repo.path(),
            &label,
            &session,
            &new.to_string_lossy(),
        )
        .unwrap();
        append(&old, "after\n");
        assert!(tail.read(&stored).unwrap_err().contains("binding changed"));
    }

    /// A person's prompt, taken idle or busy, is input; hook feedback, a
    /// compaction summary, a task notification and a tool result are not.
    #[test]
    fn only_a_prompt_is_input() {
        assert_eq!(
            input(Tool::Claude, &claude_user("hi")).as_deref(),
            Some("hi")
        );
        assert_eq!(
            input(Tool::Claude, &claude_enqueue("hi")).as_deref(),
            Some("hi")
        );
        assert_eq!(input(Tool::Codex, &codex_user("hi")).as_deref(), Some("hi"));
        let not = [
            r#"{"type":"user","isMeta":true,"message":{"content":"hi"}}"#.to_string(),
            r#"{"type":"user","isCompactSummary":true,"message":{"content":"hi"}}"#.to_string(),
            r#"{"type":"user","origin":{"kind":"task-notification"},"message":{"content":"hi"}}"#
                .to_string(),
            claude_enqueue("<task-notification>\nhi"),
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"hi"}]}}"#
                .to_string(),
            r#"{"type":"queue-operation","operation":"remove","content":"hi"}"#.to_string(),
        ];
        for line in not {
            assert_eq!(input(Tool::Claude, &line), None, "{line}");
        }
        assert_eq!(input(Tool::Codex, &claude_user("hi")), None);
    }

    /// Claude's paste wrapping comes off, at the start, in the middle with
    /// the line breaks it adds, or around the whole text.
    #[test]
    fn claudes_paste_wrapping_comes_off() {
        let body = "line one\nline two";
        assert_eq!(unwrap_pasted(&pasted("1a09", body)), body);
        let middle = format!("lead\n\n{}\ntail", pasted("00ff", body));
        assert_eq!(unwrap_pasted(&middle), format!("lead{body}tail"));
        let foreign = "<pasted_content id=\"XYZW\">\nx\n</pasted_content id=\"XYZW\">";
        assert_eq!(unwrap_pasted(foreign), foreign);
    }

    /// The whole text is the payload, a proper suffix of it is the payload
    /// with its start lost, and anything else is not the payload, however
    /// much of it agrees.
    #[test]
    fn a_record_is_attributed_by_its_whole_text() {
        let payload = "Experiment rules:\n- one\n- two";
        assert_eq!(attribute(payload, payload), Some(Delivered::Whole));
        assert_eq!(
            attribute(payload, &pasted("1a09", payload)),
            Some(Delivered::Whole)
        );
        assert_eq!(
            attribute(payload, "xperiment rules:\n- one\n- two"),
            Some(Delivered::Altered)
        );
        for other in [
            "Experiment rules:\n- one\n- three",
            "Experiment rules:",
            "Experiment rules:\n- one\n- two\n- more",
            "",
        ] {
            assert_ne!(
                attribute(payload, other),
                Some(Delivered::Whole),
                "{other:?}"
            );
        }
        assert_eq!(
            attribute(payload, "Experiment rules:\n- one\n- three"),
            None
        );
        assert_eq!(attribute(payload, ""), None);
    }
}
