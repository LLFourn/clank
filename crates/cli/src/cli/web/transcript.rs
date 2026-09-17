//! What an agent said, read from the harness's own transcript.
//!
//! `zellij subscribe` hands us rendered cells — the output of each
//! tool's TUI. Two harnesses also keep the conversation itself, as
//! JSONL clank can read: Claude Code, one file per session, one line
//! per completed content block; codex, one rollout per session, one
//! `response_item` per completed item. Neither streams tokens, so a
//! transcript is near-live: a paragraph lands when it is finished.
//!
//! Both reduce to one shape, [`Turn`], through one adapter each. The
//! adapters are pure over a line and tested on lines shaped from the
//! real files on the machine this was built on, with the content
//! redacted.

use std::path::{Path, PathBuf};

use clank_core::vocab::Tool;

/// Who a turn belongs to. `Harness` is the tool talking to its own
/// model — a stop-hook nudge, an environment note — which must not
/// read as the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Who {
    Person,
    Agent,
    Harness,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Body {
    /// Prose, with the markdown it was written as already rendered.
    /// Rendering happens HERE because only here can it be done
    /// safely: `html::render_markdown` escapes raw HTML and checks
    /// every destination against an allow-list, and the page sets
    /// this with `innerHTML`. `text` stays the source of truth;
    /// `html` is derived from it by [`Body::prose`].
    Text {
        text: String,
        #[serde(default)]
        html: String,
    },
    Thinking {
        text: String,
    },
    /// A tool call and, once it has one, its output — ONE turn, so
    /// the output arriving later is an upsert of the same id. Images
    /// in the output ride with it.
    Tool {
        name: String,
        input: String,
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
    },
    /// A picture pasted or returned, as the transcript keeps it.
    Image {
        #[serde(flatten)]
        image: Image,
    },
}

impl Body {
    /// A prose turn: the text as written, and as rendered.
    pub(crate) fn prose(text: String) -> Self {
        Body::Text {
            html: crate::cli::html::render_markdown(&text),
            text,
        }
    }
}

/// One image out of a transcript: its type and base64 data — or, over
/// [`IMAGE_CAP`] decoded, no data and its size, so the page can name
/// what it will not carry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Image {
    pub(crate) media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<String>,
    pub(crate) bytes: usize,
}

/// The largest image shipped to the page, decoded.
pub(crate) const IMAGE_CAP: usize = 2 * 1024 * 1024;

impl Image {
    fn new(media_type: &str, base64: &str) -> Self {
        let bytes = base64.trim_end_matches('=').len() * 3 / 4;
        Self {
            media_type: media_type.to_string(),
            data: (bytes <= IMAGE_CAP).then(|| base64.to_string()),
            bytes,
        }
    }

    /// A `data:<type>;base64,<data>` URL — codex's shape; anything
    /// else is not an image the transcript carries.
    fn from_data_url(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("data:")?;
        let (media_type, data) = rest.split_once(";base64,")?;
        Some(Self::new(media_type, data))
    }
}

/// Every image in a content list, in either harness's shape: a
/// claude `image` block with a base64 source, or a codex
/// `input_image` with a data URL.
fn images_in(v: &serde_json::Value) -> Vec<Image> {
    let Some(blocks) = v.as_array() else {
        return Vec::new();
    };
    blocks.iter().filter_map(image_block).collect()
}

fn image_block(b: &serde_json::Value) -> Option<Image> {
    match b.get("type").and_then(|t| t.as_str())? {
        "image" => {
            let source = b.get("source")?;
            if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
                return None;
            }
            Some(Image::new(
                source.get("media_type")?.as_str()?,
                source.get("data")?.as_str()?,
            ))
        }
        "input_image" => Image::from_data_url(b.get("image_url")?.as_str()?),
        _ => None,
    }
}

/// One thing said. `id` is the harness's own identity for it — a
/// Claude line's `uuid` (or the tool block's id, so its result can
/// find it), a codex item's `id` (or the call id, likewise) — which
/// is what lets a replay be an upsert rather than a duplicate.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Turn {
    pub(crate) id: String,
    /// Epoch seconds, from the line's own timestamp. `None` when the
    /// line carried none the parser could read.
    pub(crate) at: Option<i64>,
    pub(crate) who: Who,
    pub(crate) body: Body,
}

/// What one transcript line contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Parsed {
    Turn(Turn),
    /// A tool's output, for the earlier `Tool` turn with this id.
    ToolOutput {
        id: String,
        output: String,
        images: Vec<Image>,
    },
}

/// Whether clank can read this harness's transcript at all.
pub(crate) fn has_adapter(tool: Tool) -> bool {
    matches!(tool, Tool::Claude | Tool::Codex)
}

/// Every turn or output on one line, for `tool`. A harness without
/// an adapter, and every line that is bookkeeping, yields nothing.
pub(crate) fn parse(tool: Tool, line: &str) -> Vec<Parsed> {
    match tool {
        Tool::Claude => claude::parse(line),
        Tool::Codex => codex::parse(line),
        _ => Vec::new(),
    }
}

fn epoch(ts: Option<&str>) -> Option<i64> {
    let ts = ts?;
    time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// The text of a content that is either a string or a list of
/// `{ text }` blocks, joined.
fn text_of(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Text the harness injected rather than the person typed. Each
/// harness has a known set of elements it opens such notes with; a
/// person's own text that happens to begin with `<` — an HTML
/// snippet, a comparison — is the person's (codex on 056a342). The
/// element's NAME is what is known: codex's wake envelope is
/// `<hook_prompt hook_run_id="…">`, which no literal tag matched
/// (codex on eb143e7).
fn is_harness_text(text: &str) -> bool {
    const NAMES: [&str; 8] = [
        "system-reminder",
        "task-notification",
        "local-command-caveat",
        "command-name",
        "environment_context",
        "user_instructions",
        "permissions",
        "hook_prompt",
    ];
    let Some(rest) = text.trim_start().strip_prefix('<') else {
        return false;
    };
    let name = rest
        .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .next()
        .unwrap_or("");
    NAMES.contains(&name)
}

mod claude {
    use super::*;

    pub(super) fn parse(line: &str) -> Vec<Parsed> {
        let Ok(e) = serde_json::from_str::<serde_json::Value>(line) else {
            return Vec::new();
        };
        // Subagent traffic is a different conversation.
        if e.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
            return Vec::new();
        }
        let (Some(kind), Some(uuid), Some(message)) = (
            e.get("type").and_then(|v| v.as_str()),
            e.get("uuid").and_then(|v| v.as_str()),
            e.get("message"),
        ) else {
            return Vec::new();
        };
        let at = epoch(e.get("timestamp").and_then(|v| v.as_str()));
        let Some(content) = message.get("content") else {
            return Vec::new();
        };
        match kind {
            "assistant" => assistant(uuid, at, content),
            "user" => user(uuid, at, content),
            _ => Vec::new(),
        }
    }

    fn assistant(uuid: &str, at: Option<i64>, content: &serde_json::Value) -> Vec<Parsed> {
        let Some(blocks) = content.as_array() else {
            return Vec::new();
        };
        blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let id = format!("{uuid}:{i}");
                let body = match b.get("type").and_then(|t| t.as_str())? {
                    "text" => Body::prose(b.get("text")?.as_str()?.to_string()),
                    "thinking" => Body::Thinking {
                        text: b.get("thinking")?.as_str()?.to_string(),
                    },
                    "tool_use" => {
                        // The block's own id, so the result can find it.
                        return Some(Parsed::Turn(Turn {
                            id: b.get("id")?.as_str()?.to_string(),
                            at,
                            who: Who::Agent,
                            body: Body::Tool {
                                name: b.get("name")?.as_str()?.to_string(),
                                input: b
                                    .get("input")
                                    .map(|i| match i.as_str() {
                                        Some(s) => s.to_string(),
                                        None => i.to_string(),
                                    })
                                    .unwrap_or_default(),
                                output: None,
                                images: Vec::new(),
                            },
                        }));
                    }
                    _ => return None,
                };
                Some(Parsed::Turn(Turn {
                    id,
                    at,
                    who: Who::Agent,
                    body,
                }))
            })
            .collect()
    }

    fn user(uuid: &str, at: Option<i64>, content: &serde_json::Value) -> Vec<Parsed> {
        match content {
            serde_json::Value::String(text) => vec![Parsed::Turn(Turn {
                id: uuid.to_string(),
                at,
                who: if is_harness_text(text) {
                    Who::Harness
                } else {
                    Who::Person
                },
                body: Body::prose(text.clone()),
            })],
            serde_json::Value::Array(blocks) => {
                // A typed message is a string — unless a picture was
                // pasted with it, which makes it a list of the text
                // and the image, both the person's. Any other list is
                // the harness's: tool results, and notes beside them.
                let pasted = blocks.iter().any(|b| image_block(b).is_some());
                blocks
                    .iter()
                    .enumerate()
                    .filter_map(|(i, b)| match b.get("type").and_then(|t| t.as_str())? {
                        "tool_result" => Some(Parsed::ToolOutput {
                            id: b.get("tool_use_id")?.as_str()?.to_string(),
                            output: b.get("content").map(text_of).unwrap_or_default(),
                            images: b.get("content").map(images_in).unwrap_or_default(),
                        }),
                        "text" => {
                            let text = b.get("text")?.as_str()?.to_string();
                            let who = if pasted && !is_harness_text(&text) {
                                Who::Person
                            } else {
                                Who::Harness
                            };
                            Some(Parsed::Turn(Turn {
                                id: format!("{uuid}:{i}"),
                                at,
                                who,
                                body: Body::prose(text),
                            }))
                        }
                        "image" => Some(Parsed::Turn(Turn {
                            id: format!("{uuid}:{i}"),
                            at,
                            who: Who::Person,
                            body: Body::Image {
                                image: image_block(b)?,
                            },
                        })),
                        _ => None,
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

mod codex {
    use super::*;

    pub(super) fn parse(line: &str) -> Vec<Parsed> {
        let Ok(e) = serde_json::from_str::<serde_json::Value>(line) else {
            return Vec::new();
        };
        if e.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            return Vec::new();
        }
        let Some(p) = e.get("payload") else {
            return Vec::new();
        };
        let at = epoch(e.get("timestamp").and_then(|v| v.as_str()));
        let id = |fallback: &str| {
            p.get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "ord{}-{fallback}",
                        e.get("ordinal").and_then(|v| v.as_u64()).unwrap_or(0)
                    )
                })
        };
        let call_id = || {
            p.get("call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(str::to_string);
        match p.get("type").and_then(|v| v.as_str()) {
            Some("message") => {
                let msg = id("msg");
                let user = s("role").as_deref() == Some("user");
                let who = |text: &str| match (user, is_harness_text(text)) {
                    (true, true) => Who::Harness,
                    (true, false) => Who::Person,
                    (false, _) => Who::Agent,
                };
                let text_turn = |id: String, text: String| {
                    Parsed::Turn(Turn {
                        id,
                        at,
                        who: who(&text),
                        body: Body::prose(text),
                    })
                };
                // A list is walked in order, one turn per block, so a
                // picture stays beside the words it came with (codex
                // on 219a1bc); a bare string is the message.
                let Some(blocks) = p.get("content").and_then(|c| c.as_array()) else {
                    let text = p.get("content").map(text_of).unwrap_or_default();
                    return vec![text_turn(msg, text)];
                };
                let out: Vec<Parsed> = blocks
                    .iter()
                    .enumerate()
                    .filter_map(|(i, b)| {
                        if let Some(image) = image_block(b) {
                            return Some(Parsed::Turn(Turn {
                                id: format!("{msg}:{i}"),
                                at,
                                who: who(""),
                                body: Body::Image { image },
                            }));
                        }
                        let text = b.get("text")?.as_str()?;
                        (!text.is_empty())
                            .then(|| text_turn(format!("{msg}:{i}"), text.to_string()))
                    })
                    .collect();
                if out.is_empty() {
                    // Nothing readable in the list: the message still
                    // happened, so its (empty) words stand in.
                    return vec![text_turn(msg, String::new())];
                }
                out
            }
            Some("reasoning") => {
                let text = p.get("summary").map(text_of).unwrap_or_default();
                if text.is_empty() {
                    return Vec::new();
                }
                vec![Parsed::Turn(Turn {
                    id: id("rs"),
                    at,
                    who: Who::Agent,
                    body: Body::Thinking { text },
                })]
            }
            Some(kind @ ("custom_tool_call" | "function_call")) => {
                let Some(cid) = call_id() else {
                    return Vec::new();
                };
                let input = if kind == "custom_tool_call" {
                    s("input")
                } else {
                    s("arguments")
                }
                .unwrap_or_default();
                vec![Parsed::Turn(Turn {
                    id: cid,
                    at,
                    who: Who::Agent,
                    body: Body::Tool {
                        name: s("name").unwrap_or_else(|| "tool".into()),
                        input,
                        output: None,
                        images: Vec::new(),
                    },
                })]
            }
            Some("custom_tool_call_output" | "function_call_output") => {
                let Some(cid) = call_id() else {
                    return Vec::new();
                };
                vec![Parsed::ToolOutput {
                    id: cid,
                    output: p.get("output").map(text_of).unwrap_or_default(),
                    images: p.get("output").map(images_in).unwrap_or_default(),
                }]
            }
            _ => Vec::new(),
        }
    }
}

/// Where a bound session's transcript is, or `None` when there is
/// none to read. A recorded path (Claude's SessionStart says it)
/// wins; Claude without one falls back to the derivation the harness
/// uses; codex is found by its session id under its sessions dir.
pub(crate) fn transcript_path(
    tool: Tool,
    session_id: &str,
    recorded: Option<&Path>,
    cwd: &Path,
    home: &Path,
) -> Option<PathBuf> {
    match tool {
        Tool::Claude => Some(
            recorded
                .map(Path::to_path_buf)
                .unwrap_or_else(|| claude_derived(session_id, cwd, home)),
        ),
        Tool::Codex => {
            let mut found = Vec::new();
            codex_candidates(
                &home.join(".codex").join("sessions"),
                session_id,
                &mut found,
            );
            pick_newest(found)
        }
        _ => None,
    }
}

/// `~/.claude/projects/<cwd with every separator as '-'>/<id>.jsonl`.
fn claude_derived(session_id: &str, cwd: &Path, home: &Path) -> PathBuf {
    let slug: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    home.join(".claude")
        .join("projects")
        .join(slug)
        .join(format!("{session_id}.jsonl"))
}

/// Every `rollout-*-<id>.jsonl` under the sessions dir — walked, not
/// globbed, so nothing new is depended on.
fn codex_candidates(dir: &Path, session_id: &str, out: &mut Vec<(PathBuf, std::time::SystemTime)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            codex_candidates(&path, session_id, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("rollout-")
            && name.ends_with(&format!("-{session_id}.jsonl"))
            && let Ok(modified) = entry.metadata().and_then(|m| m.modified())
        {
            out.push((path, modified));
        }
    }
}

/// Several files for one id (a resumed session writes a new rollout)
/// — the newest is the live one.
fn pick_newest(mut found: Vec<(PathBuf, std::time::SystemTime)>) -> Option<PathBuf> {
    found.sort_by(|a, b| b.1.cmp(&a.1));
    found.into_iter().next().map(|(p, _)| p)
}

/// A transcript being followed: only what is appended is read, and a
/// file that shrank (compaction, replacement) is re-read from its
/// tail with `reset` set so the reader can start a new generation.
pub(crate) struct Tail {
    path: PathBuf,
    pos: u64,
    partial: Vec<u8>,
    window: u64,
}

pub(crate) struct Polled {
    pub(crate) lines: Vec<String>,
    pub(crate) reset: bool,
}

impl Tail {
    /// Open at the file's end, returning the whole lines within the
    /// last `window` bytes — the files run to hundreds of megabytes
    /// and the page wants the last few screens, not the history.
    pub(crate) fn open(path: &Path, window: u64) -> std::io::Result<(Self, Vec<String>)> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        // The cursor is where the read actually ended, not the length
        // sampled before it: bytes appended during the read would
        // otherwise be read twice (codex on 056a342).
        let pos = start + buf.len() as u64;
        // Whatever precedes the first newline in a mid-file window is
        // the tail of a line we did not read the head of. Whatever
        // follows the LAST newline is a record still being written —
        // kept, so it completes on a later poll rather than vanishing.
        let body: &[u8] = if start > 0 {
            match buf.iter().position(|b| *b == b'\n') {
                Some(i) => &buf[i + 1..],
                None => &[],
            }
        } else {
            &buf
        };
        let (lines, partial) = whole_lines(body);
        Ok((
            Self {
                path: path.to_path_buf(),
                pos,
                partial,
                window,
            },
            lines,
        ))
    }

    pub(crate) fn poll(&mut self) -> std::io::Result<Polled> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&self.path)?;
        let len = file.metadata()?.len();
        if len < self.pos {
            let (fresh, lines) = Self::open(&self.path, self.window)?;
            *self = fresh;
            return Ok(Polled { lines, reset: true });
        }
        if len == self.pos {
            return Ok(Polled {
                lines: Vec::new(),
                reset: false,
            });
        }
        file.seek(SeekFrom::Start(self.pos))?;
        let mut fresh = Vec::new();
        file.read_to_end(&mut fresh)?;
        self.pos += fresh.len() as u64;
        let mut buf = std::mem::take(&mut self.partial);
        buf.extend_from_slice(&fresh);
        let (lines, rest) = whole_lines(&buf);
        self.partial = rest;
        Ok(Polled {
            lines,
            reset: false,
        })
    }
}

/// Complete lines, and the bytes after the last newline — a line
/// still being written is not parsed until it ends.
fn whole_lines(buf: &[u8]) -> (Vec<String>, Vec<u8>) {
    let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else {
        return (Vec::new(), buf.to_vec());
    };
    let lines = String::from_utf8_lossy(&buf[..last_nl])
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    (lines, buf[last_nl + 1..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = include_str!("fixtures/claude.jsonl");
    const CODEX: &str = include_str!("fixtures/codex.jsonl");

    /// An oracle built from the calendar, not parsed from a string —
    /// so a parser reading the wrong field cannot agree with it by
    /// construction.
    fn utc(y: i32, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> i64 {
        time::Date::from_calendar_date(y, time::Month::try_from(mo).unwrap(), d)
            .unwrap()
            .with_hms(h, mi, s)
            .unwrap()
            .assume_utc()
            .unix_timestamp()
    }

    fn all(tool: Tool, fixture: &str) -> Vec<Parsed> {
        fixture.lines().flat_map(|l| parse(tool, l)).collect()
    }

    fn turn(p: &Parsed) -> &Turn {
        match p {
            Parsed::Turn(t) => t,
            other => panic!("expected a turn, got {other:?}"),
        }
    }

    /// The person, the agent's thinking, its tool call, the tool's
    /// result finding that call by id, its answer — and the two kinds
    /// of line that must NOT read as the person: a system-reminder
    /// string and a text block in a user list are the harness. A
    /// sidechain line and bookkeeping yield nothing.
    #[test]
    fn claude_lines_become_turns_and_the_harness_is_not_the_person() {
        let got = all(Tool::Claude, CLAUDE);
        assert_eq!(got.len(), 9, "{got:#?}");
        let t = turn(&got[0]);
        assert_eq!((t.who, &t.id), (Who::Person, &"u-1".to_string()));
        assert_eq!(
            t.at,
            Some(utc(2026, 9, 11, 0, 22, 6)),
            "the line's own timestamp"
        );
        assert!(matches!(&t.body, Body::Text { text, .. } if text.starts_with("fix the layout")));
        assert!(
            matches!(&turn(&got[1]).body, Body::Thinking { text } if text == "The extent decides.")
        );
        let tool = turn(&got[2]);
        assert_eq!(
            tool.id, "toolu_01",
            "the block's id, so its result can find it"
        );
        assert!(
            matches!(&tool.body, Body::Tool { name, input, output: None, .. } if name == "Bash" && input.contains("cargo test"))
        );
        assert_eq!(
            got[3],
            Parsed::ToolOutput {
                id: "toolu_01".into(),
                output: "test result: ok. 1296 passed".into(),
                images: vec![]
            }
        );
        assert!(
            matches!(&turn(&got[4]).body, Body::Text { text, .. } if text.starts_with("Fixed"))
        );
        assert_eq!(
            turn(&got[5]).who,
            Who::Harness,
            "a stop-hook nudge is the harness"
        );
        assert_eq!(
            turn(&got[6]).who,
            Who::Harness,
            "an interruption note is the harness"
        );
        assert!(
            !got.iter().any(|p| matches!(p, Parsed::Turn(t) if matches!(&t.body, Body::Text { text, .. } if text.contains("subagent")))),
            "sidechain lines are another conversation"
        );
    }

    /// Codex: a user `input_text` is the person unless it is a tagged
    /// injection; the two tool-call families become one `Tool`; an
    /// empty reasoning summary is nothing; bookkeeping is nothing.
    #[test]
    fn codex_items_become_turns_by_the_same_rules() {
        let got = all(Tool::Codex, CODEX);
        assert_eq!(got.len(), 11, "{got:#?}");
        assert_eq!(
            (turn(&got[0]).who, turn(&got[0]).id.as_str()),
            (Who::Person, "msg_u1:0")
        );
        assert_eq!(
            turn(&got[1]).who,
            Who::Harness,
            "environment context is the harness"
        );
        assert!(
            matches!(&turn(&got[2]).body, Body::Thinking { text } if text == "Checking the premise.")
        );
        let exec = turn(&got[3]);
        assert_eq!(exec.id, "call_A");
        assert!(
            matches!(&exec.body, Body::Tool { name, input, .. } if name == "exec" && input == "cargo test -p clank")
        );
        assert_eq!(
            got[4],
            Parsed::ToolOutput {
                id: "call_A".into(),
                output: "Script completed\n\n1296 passed".into(),
                images: vec![]
            }
        );
        assert!(
            matches!(&turn(&got[5]).body, Body::Tool { name, input, .. } if name == "shell" && input.contains("ls"))
        );
        assert_eq!(
            got[6],
            Parsed::ToolOutput {
                id: "call_B".into(),
                output: "a b c".into(),
                images: vec![]
            }
        );
        assert!(
            matches!(&turn(&got[7]).body, Body::Text { text, .. } if text.starts_with("CONTINUE"))
        );
        assert_eq!(turn(&got[7]).at, Some(utc(2026, 8, 27, 8, 33, 50)));
        assert_eq!(
            turn(&got[8]).who,
            Who::Harness,
            "clank's own wake, as codex records it, is the harness"
        );
    }

    /// A harness that grows its format does not kill the feed: an
    /// unknown item, an unknown block, a line that is not JSON.
    #[test]
    fn a_future_shape_yields_nothing_rather_than_an_error() {
        assert!(
            parse(
                Tool::Codex,
                r#"{"type":"response_item","payload":{"type":"web_search_call","id":"w1"}}"#
            )
            .is_empty()
        );
        assert!(
            parse(
                Tool::Claude,
                r#"{"type":"assistant","uuid":"x","message":{"content":[{"type":"hologram"}]}}"#
            )
            .is_empty()
        );
        assert!(parse(Tool::Claude, "not json").is_empty());
        assert!(
            parse(Tool::Grok, CLAUDE.lines().next().unwrap()).is_empty(),
            "no adapter, no turns"
        );
        assert!(!has_adapter(Tool::OpenCode) && has_adapter(Tool::Claude));
    }

    #[test]
    fn the_path_is_recorded_else_derived_for_claude_and_found_by_id_for_codex() {
        let home = tempfile::tempdir().unwrap();
        let cwd = Path::new("/Users/lloyd/src/clank");
        assert_eq!(
            transcript_path(
                Tool::Claude,
                "abc",
                Some(Path::new("/r/t.jsonl")),
                cwd,
                home.path()
            ),
            Some(PathBuf::from("/r/t.jsonl"))
        );
        assert_eq!(
            transcript_path(Tool::Claude, "abc", None, cwd, home.path()),
            Some(
                home.path()
                    .join(".claude/projects/-Users-lloyd-src-clank/abc.jsonl")
            )
        );
        // Codex: none, one, two (the newest wins).
        assert_eq!(
            transcript_path(Tool::Codex, "id1", None, cwd, home.path()),
            None
        );
        let day = home.path().join(".codex/sessions/2026/08/27");
        std::fs::create_dir_all(&day).unwrap();
        let older = day.join("rollout-2026-08-27T08-33-17-id1.jsonl");
        std::fs::write(&older, "{}\n").unwrap();
        assert_eq!(
            transcript_path(Tool::Codex, "id1", None, cwd, home.path()),
            Some(older.clone())
        );
        let newer = home
            .path()
            .join(".codex/sessions/2026/09/01/rollout-2026-09-01T10-00-00-id1.jsonl");
        std::fs::create_dir_all(newer.parent().unwrap()).unwrap();
        std::fs::write(&newer, "{}\n").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&older)
            .unwrap()
            .set_modified(past)
            .unwrap();
        assert_eq!(
            transcript_path(Tool::Codex, "id1", None, cwd, home.path()),
            Some(newer)
        );
        assert_eq!(
            transcript_path(Tool::Codex, "other", None, cwd, home.path()),
            None
        );
        assert_eq!(
            transcript_path(Tool::Grok, "x", None, cwd, home.path()),
            None
        );
    }

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn png(image: &Image) -> bool {
        image.media_type == "image/png" && image.data.as_deref() == Some(PNG) && image.bytes == 70
    }

    /// A pasted picture is a turn of the person's, beside the text it
    /// came with — which is the person's too, not a harness note, list
    /// or no list; a picture in a tool's result rides on the output;
    /// one over the cap is named, not shipped.
    #[test]
    fn a_pasted_image_is_a_turn_of_the_persons() {
        let line = |content: serde_json::Value| {
            format!(
                r#"{{"type":"user","uuid":"u","timestamp":"2026-09-11T00:00:00Z","message":{{"role":"user","content":{}}}}}"#,
                content
            )
        };
        let img = serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}});
        let got = parse(
            Tool::Claude,
            &line(serde_json::json!([{"type":"text","text":"here's what I see"}, img])),
        );
        assert_eq!(got.len(), 2);
        assert_eq!(
            (turn(&got[0]).who, turn(&got[0]).id.as_str()),
            (Who::Person, "u:0")
        );
        let t = turn(&got[1]);
        assert_eq!((t.who, t.id.as_str()), (Who::Person, "u:1"));
        assert!(
            matches!(&t.body, Body::Image { image } if png(image)),
            "{t:?}"
        );

        let got = parse(
            Tool::Claude,
            &line(
                serde_json::json!([{"type":"tool_result","tool_use_id":"toolu_9","content":[{"type":"text","text":"a picture"}, img]}]),
            ),
        );
        match &got[0] {
            Parsed::ToolOutput { id, output, images } => {
                assert_eq!((id.as_str(), output.as_str()), ("toolu_9", "a picture"));
                assert_eq!(images.len(), 1);
                assert!(png(&images[0]));
            }
            other => panic!("{other:?}"),
        }

        let big = "A".repeat(IMAGE_CAP / 3 * 4 + 400);
        let got = parse(
            Tool::Claude,
            &line(
                serde_json::json!([{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":big}}]),
            ),
        );
        match &turn(&got[0]).body {
            Body::Image { image } => {
                assert_eq!(image.data, None, "over the cap: not shipped");
                assert!(image.bytes > IMAGE_CAP);
                assert_eq!(image.media_type, "image/jpeg");
            }
            other => panic!("{other:?}"),
        }
        // A harness note in a list stays the harness's: no picture there.
        let got = parse(
            Tool::Claude,
            &line(serde_json::json!([{"type":"text","text":"note"}])),
        );
        assert_eq!(turn(&got[0]).who, Who::Harness);
    }

    /// codex: an `input_image` with a data URL is a picture of the
    /// person's beside their words; any other URL is nothing.
    #[test]
    fn a_codex_input_image_is_a_turn_of_the_persons() {
        let line = |content: serde_json::Value| {
            format!(
                r#"{{"timestamp":"2026-08-27T08:33:20.000Z","ordinal":1,"type":"response_item","payload":{{"type":"message","id":"m7","role":"user","content":{}}}}}"#,
                content
            )
        };
        let got = parse(
            Tool::Codex,
            &line(serde_json::json!([
                {"type":"input_text","text":"look"},
                {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")}
            ])),
        );
        assert_eq!(got.len(), 2);
        assert_eq!(
            (turn(&got[0]).who, turn(&got[0]).id.as_str()),
            (Who::Person, "m7:0")
        );
        let t = turn(&got[1]);
        assert_eq!((t.who, t.id.as_str()), (Who::Person, "m7:1"));
        assert!(matches!(&t.body, Body::Image { image } if png(image)));
        let got = parse(
            Tool::Codex,
            &line(
                serde_json::json!([{"type":"input_image","image_url":"https://example.com/x.png"}]),
            ),
        );
        assert_eq!(
            got.len(),
            1,
            "no picture, and the message's (empty) words stand in"
        );
        assert_eq!(turn(&got[0]).id, "m7");
        assert!(matches!(&turn(&got[0]).body, Body::Text { text, .. } if text.is_empty()));

        // Mixed content keeps its order: a caption, its picture, the
        // next caption, its picture — each by its block.
        let got = parse(
            Tool::Codex,
            &line(serde_json::json!([
                {"type":"input_text","text":"before"},
                {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")},
                {"type":"input_text","text":"after"},
                {"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")}
            ])),
        );
        let kinds: Vec<(String, &str)> = got
            .iter()
            .map(|p| {
                let t = turn(p);
                (
                    t.id.clone(),
                    match &t.body {
                        Body::Text { text, .. } => text.as_str(),
                        Body::Image { .. } => "<image>",
                        _ => "?",
                    },
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("m7:0".to_string(), "before"),
                ("m7:1".to_string(), "<image>"),
                ("m7:2".to_string(), "after"),
                ("m7:3".to_string(), "<image>"),
            ]
        );
        // A bare string is the message, under the message's id.
        let got = parse(Tool::Codex, &line(serde_json::json!("plain")));
        assert_eq!(
            (turn(&got[0]).id.as_str(), turn(&got[0]).who),
            ("m7", Who::Person)
        );
    }

    /// A person's text that begins with `<` is still the person's:
    /// only the harnesses' own tags mark a note as theirs.
    #[test]
    fn only_known_tags_make_a_line_the_harness() {
        let line = |text: &str| {
            format!(
                r#"{{"type":"user","uuid":"u","timestamp":"2026-09-11T00:00:00Z","message":{{"role":"user","content":{}}}}}"#,
                serde_json::json!(text)
            )
        };
        assert_eq!(
            turn(&parse(Tool::Claude, &line("<b>bold</b> looks wrong"))[0]).who,
            Who::Person
        );
        assert_eq!(
            turn(&parse(Tool::Claude, &line("  <system-reminder>\nnudge"))[0]).who,
            Who::Harness
        );
        assert_eq!(
            turn(&parse(Tool::Claude, &line("<task-notification>x"))[0]).who,
            Who::Harness
        );
        assert_eq!(
            turn(&parse(Tool::Claude, &line("a < b"))[0]).who,
            Who::Person
        );
        // The name marks the element whatever attributes follow it;
        // an unknown element with attributes is still the person's.
        let codex_user = |text: &str| {
            format!(
                r#"{{"timestamp":"2026-08-27T08:33:20.000Z","ordinal":1,"type":"response_item","payload":{{"type":"message","id":"m","role":"user","content":[{{"type":"input_text","text":{}}}]}}}}"#,
                serde_json::json!(text)
            )
        };
        let wake = r#"<hook_prompt hook_run_id="stop:1:/h/.codex/hooks.json">Clank wait returned work</hook_prompt>"#;
        assert_eq!(
            turn(&parse(Tool::Codex, &codex_user(wake))[0]).who,
            Who::Harness
        );
        assert_eq!(
            turn(&parse(Tool::Codex, &codex_user(r#"<div class="x">my html</div>"#))[0]).who,
            Who::Person
        );
        assert_eq!(
            turn(&parse(Tool::Codex, &codex_user("<hook_prompting> me"))[0]).who,
            Who::Person
        );
    }

    /// Opening while a writer has emitted only the head of a record
    /// must not lose that record: the partial is kept, and the whole
    /// line arrives exactly once when the rest lands. And the cursor
    /// is where the read ended, so nothing is read twice.
    #[test]
    fn opening_on_a_partial_record_keeps_it_for_the_next_poll() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"n":1}}"#).unwrap();
        write!(f, r#"{{"n":2,"text":"half"#).unwrap();
        f.flush().unwrap();
        let (mut tail, lines) = Tail::open(&path, 1 << 20).unwrap();
        assert_eq!(lines, vec![r#"{"n":1}"#], "only the whole line at open");
        write!(f, r#" of it"}}"#).unwrap();
        writeln!(f).unwrap();
        f.flush().unwrap();
        let p = tail.poll().unwrap();
        assert_eq!(
            p.lines,
            vec![r#"{"n":2,"text":"half of it"}"#],
            "the record, once, whole"
        );
        assert!(tail.poll().unwrap().lines.is_empty(), "and never again");
    }

    /// The tail: the last window's whole lines at open (a mid-window
    /// partial line dropped), then only what is appended, a line
    /// without its newline held back, and a shrink re-read as a reset.
    #[test]
    fn the_tail_reads_the_window_then_only_what_is_appended() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..50 {
            writeln!(f, r#"{{"n":{i},"pad":"{}"}}"#, "x".repeat(40)).unwrap();
        }
        let (mut tail, lines) = Tail::open(&path, 300).unwrap();
        assert!(
            lines.len() < 50 && lines.len() >= 3,
            "a window of the end: {}",
            lines.len()
        );
        assert!(
            lines[0].starts_with('{') && lines[0].ends_with('}'),
            "no half line: {}",
            lines[0]
        );
        assert!(lines.last().unwrap().contains("\"n\":49"));

        assert!(
            tail.poll().unwrap().lines.is_empty(),
            "nothing appended, nothing read"
        );
        write!(f, r#"{{"n":50}}"#).unwrap();
        f.flush().unwrap();
        assert!(
            tail.poll().unwrap().lines.is_empty(),
            "no newline yet: held back"
        );
        writeln!(f).unwrap();
        writeln!(f, r#"{{"n":51}}"#).unwrap();
        f.flush().unwrap();
        let p = tail.poll().unwrap();
        assert_eq!(p.lines, vec![r#"{"n":50}"#, r#"{"n":51}"#]);
        assert!(!p.reset);

        std::fs::write(&path, "{\"n\":0}\n").unwrap();
        let p = tail.poll().unwrap();
        assert!(p.reset, "a shrink is a new generation");
        assert_eq!(p.lines, vec![r#"{"n":0}"#]);
    }
}
