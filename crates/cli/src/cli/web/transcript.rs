//! What an agent said, read from the harness's own transcript.
//!
//! `zellij subscribe` hands us rendered cells — the output of each
//! tool's TUI. Three harnesses also keep the conversation itself, as
//! JSONL clank can read: Claude Code, one file per session, one line
//! per completed content block; codex, one rollout per session, one
//! `response_item` per completed item; Grok, one chat history per
//! session. These files do not stream tokens, so a
//! transcript is near-live: a paragraph lands when it is finished.
//!
//! All reduce to one shape, [`Turn`], through one adapter each. The
//! adapters are pure over a line and tested on lines shaped from the
//! real files on the machine this was built on, with the content
//! redacted.

use std::path::{Path, PathBuf};

use clank_core::vocab::Tool;

mod grok_task;
pub(super) mod questions;
pub(crate) use grok_task::BackgroundTask;
pub(crate) use grok_task::apply as apply_tool_output;

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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tasks: Vec<BackgroundTask>,
        /// Read-only recorded question content. A transcript never grants
        /// permission to answer a live provider request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        question: Option<questions::Question>,
        name: String,
        input: String,
        /// What the call SAYS it is doing: one line, derived from the
        /// input by [`told`]. Empty on a turn recorded before this
        /// existed, which the page draws as it used to.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        /// The call as something to read — a command as its own text,
        /// not a JSON string literal with `\n` in it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        detail: String,
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

/// What a tool call says it is doing, and the call itself to read.
pub(crate) struct Told {
    pub(crate) title: String,
    pub(crate) detail: String,
}

/// The field that IS the point of a call, per tool. Knowledge about
/// tools, kept where it can be tested rather than in a slice index in
/// a `<script>`: Claude's shell calls carry a `description` written
/// for a reader, and every other tool has one field that says what
/// the call is about.
fn headline(name: &str) -> &'static [&'static str] {
    match name {
        "Bash" | "BashOutput" => &["description", "command"],
        "Task" => &["description", "prompt"],
        "Read" | "Edit" | "Write" => &["file_path"],
        "NotebookEdit" => &["notebook_path"],
        "Glob" | "Grep" => &["pattern"],
        "WebFetch" => &["url"],
        "WebSearch" => &["query"],
        // codex's own, and the shells an MCP server tends to expose.
        "shell" | "exec" | "local_shell" | "container.exec" => &["command", "cmd"],
        // A tool nobody taught this: the fields that usually say what
        // a call is for, before falling back to whatever it has.
        _ => &[
            "description",
            "command",
            "cmd",
            "query",
            "prompt",
            "url",
            "file_path",
            "path",
        ],
    }
}

/// A value as a TITLE: a line someone can READ. A string is itself
/// and a list of strings is the command line it spells; everything
/// else — a structure, a count, a flag — is not a title.
///
/// `{"status":"open"}` as a title spends the row on the shape of the
/// call instead of its point (codex on b09ad51), and `20` or `false`
/// spends it on whichever piece of metadata sorted first: a call
/// titled `20` says less than the tool's own name (codex on b3725e8).
/// They stay in the detail, where a number is worth reading.
fn as_title(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let words: Vec<&str> = items.iter().filter_map(|i| i.as_str()).collect();
            (words.len() == items.len() && !words.is_empty()).then(|| words.join(" "))
        }
        _ => None,
    }
}

/// A value in the DETAIL, which keeps EVERYTHING. What the title
/// could not say is exactly what someone expands a call to read, so a
/// structure is its JSON and a null is a null — dropping either left
/// a search whose query was gone from both lines (codex on b09ad51).
fn in_detail(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// How much of a title a row can hold; the detail keeps the rest.
const TITLE_MAX: usize = 120;

/// One line, cut to fit, with the cut shown.
fn to_a_line(s: &str) -> String {
    let line = s
        .split('\n')
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    match line.char_indices().nth(TITLE_MAX) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// What to show for a call to `name` made with `input`.
///
/// The title is the call's own sentence where it has one, and never
/// the object it was called with: a row that reads
/// `{"command":"for f in .clank/agents/…` has spent its line before
/// reaching the verb (a-tool-call-says-what-it-did).
pub(crate) fn told(name: &str, input: &str) -> Told {
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str::<serde_json::Value>(input)
    else {
        // Not an object: codex's shell input is the command itself,
        // and a command is already what there is to say.
        return Told {
            title: to_a_line(input),
            detail: input.to_string(),
        };
    };
    let titled = |keys: &[&str]| {
        keys.iter().find_map(|k| {
            fields
                .get(*k)
                .and_then(as_title)
                .filter(|v| !v.trim().is_empty())
        })
    };
    if name == "run_terminal_command"
        && let Some(command) = fields.get("command").and_then(|value| value.as_str())
    {
        return Told {
            title: to_a_line(command),
            detail: command.to_owned(),
        };
    }
    if name == "get_command_or_subagent_output" {
        return Told {
            title: "Read background output".into(),
            detail: input.into(),
        };
    }
    let title = titled(headline(name))
        // Whatever it does have that reads as a line, so a tool nobody
        // taught this still says something; and its own name when
        // nothing does, rather than its braces.
        .or_else(|| {
            fields
                .values()
                .filter_map(as_title)
                .find(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| name.to_string());
    // A command that IS a string reads as itself; an argv does not,
    // because joining it with spaces loses where its arguments ended
    // — `["printf", "%s", "a b"]` and `["printf", "%s", "a", "b"]`
    // are different calls that ran differently, and the expansion is
    // where that difference has to survive (codex on b3725e8).
    let sole_command = (fields.len() == 1)
        .then(|| {
            ["command", "cmd"]
                .iter()
                .find_map(|k| fields.get(*k).and_then(|v| v.as_str()))
        })
        .flatten();
    // Everything else is its fields, one to a line and NONE left out,
    // so the expansion is the call rather than the part of it that
    // formatted nicely.
    let detail = match sole_command {
        Some(command) => command.to_string(),
        None => fields
            .iter()
            .map(|(k, v)| format!("{k}: {}", in_detail(v)))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    Told {
        title: to_a_line(&title),
        detail,
    }
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

/// Retained original shared by feed snapshots. Only its descriptor travels
/// with turns; pixels are requested through the authenticated image action.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Image {
    pub(crate) media_type: String,
    #[serde(skip)]
    pub(crate) data: Option<std::sync::Arc<str>>,
    pub(crate) bytes: usize,
    #[serde(default)]
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) label: String,
}

impl Image {
    fn unavailable(label: String) -> Self {
        Self {
            media_type: "image/png".into(),
            data: None,
            bytes: 0,
            key: blake3::hash(label.as_bytes()).to_hex().to_string(),
            label,
        }
    }
    pub(super) fn new(media_type: &str, base64: &str) -> Self {
        let bytes = base64.trim_end_matches('=').len() * 3 / 4;
        let mut hash = blake3::Hasher::new();
        hash.update(media_type.as_bytes());
        hash.update(&[0]);
        hash.update(base64.as_bytes());
        Self {
            media_type: media_type.to_string(),
            data: Some(base64.into()),
            bytes,
            key: hash.finalize().to_hex().to_string(),
            label: "Image attachment".into(),
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
    matches!(tool, Tool::Claude | Tool::Codex | Tool::Grok)
}

/// Every turn or output on one line, for `tool`. A harness without
/// an adapter, and every line that is bookkeeping, yields nothing.
#[cfg(test)]
pub(crate) fn parse(tool: Tool, line: &str) -> Vec<Parsed> {
    parse_at(tool, line, 0)
}

/// Grok does not assign IDs to chat messages. The byte offset of a
/// complete JSONL record stays stable across the initial window and
/// later polls, even when messages have identical text.
pub(crate) fn parse_at(tool: Tool, line: &str, offset: u64) -> Vec<Parsed> {
    match tool {
        Tool::Claude => claude::parse(line),
        Tool::Codex => codex::parse(line),
        Tool::Grok => grok::parse(line, offset),
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
                        let name = b.get("name")?.as_str()?.to_string();
                        let input = b
                            .get("input")
                            .map(|i| match i.as_str() {
                                Some(s) => s.to_string(),
                                None => i.to_string(),
                            })
                            .unwrap_or_default();
                        let Told { title, detail } = told(&name, &input);
                        // The block's own id, so the result can find it.
                        return Some(Parsed::Turn(Turn {
                            id: b.get("id")?.as_str()?.to_string(),
                            at,
                            who: Who::Agent,
                            body: Body::Tool {
                                tasks: Vec::new(),
                                question: questions::parse(Tool::Claude, &name, &input),
                                name,
                                input,
                                title,
                                detail,
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

    fn attachment_label(block: &serde_json::Value) -> Option<String> {
        if block.get("type")?.as_str()? != "input_text" {
            return None;
        }
        let marker = block.get("text")?.as_str()?.trim();
        let name = marker
            .strip_prefix("<image name=")?
            .strip_suffix("\">")?
            .rsplit_once(" path=\"")?
            .0;
        if name.is_empty() || name.contains(['<', '>', '\n']) {
            return None;
        }
        Some(name.trim_matches(['[', ']']).to_owned())
    }

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
                let mut out = Vec::new();
                let mut i = 0;
                while i < blocks.len() {
                    let b = &blocks[i];
                    // The actual Codex wrapper is three adjacent structured
                    // content items. A text string merely resembling markup
                    // is never authority to hide text or read its local path.
                    if let Some(label) = attachment_label(b)
                        && blocks
                            .get(i + 1)
                            .and_then(|b| b.get("type"))
                            .and_then(|v| v.as_str())
                            == Some("input_image")
                        && blocks.get(i + 2).is_some_and(|b| {
                            b.get("type").and_then(|v| v.as_str()) == Some("input_text")
                                && b.get("text")
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|text| text.trim() == "</image>")
                        })
                    {
                        let mut image = image_block(&blocks[i + 1])
                            .unwrap_or_else(|| Image::unavailable(label.clone()));
                        image.label = label;
                        out.push(Parsed::Turn(Turn {
                            id: format!("{msg}:{}", i + 1),
                            at,
                            who: who(""),
                            body: Body::Image { image },
                        }));
                        i += 3;
                        continue;
                    }
                    if b.get("type").and_then(|v| v.as_str()) == Some("input_image") {
                        let image = image_block(b)
                            .unwrap_or_else(|| Image::unavailable("Image attachment".into()));
                        out.push(Parsed::Turn(Turn {
                            id: format!("{msg}:{i}"),
                            at,
                            who: who(""),
                            body: Body::Image { image },
                        }));
                    } else if let Some(text) = b
                        .get("text")
                        .and_then(|v| v.as_str())
                        .filter(|text| !text.is_empty())
                    {
                        out.push(text_turn(format!("{msg}:{i}"), text.to_owned()));
                    }
                    i += 1;
                }
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
                // A custom call carries its input as text; a function
                // call carries its arguments as JSON — in a STRING,
                // which is why the object has to be read out of it
                // before anything can be said about the call (codex on
                // 3ad0dfc).
                let input = if kind == "custom_tool_call" {
                    s("input")
                } else {
                    s("arguments")
                }
                .unwrap_or_default();
                let name = s("name").unwrap_or_else(|| "tool".into());
                let Told { title, detail } = told(&name, &input);
                vec![Parsed::Turn(Turn {
                    id: cid,
                    at,
                    who: Who::Agent,
                    body: Body::Tool {
                        tasks: Vec::new(),
                        question: questions::parse(Tool::Codex, &name, &input),
                        name,
                        input,
                        title,
                        detail,
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

mod grok {
    use super::*;

    pub(super) fn parse(line: &str, offset: u64) -> Vec<Parsed> {
        let Ok(e) = serde_json::from_str::<serde_json::Value>(line) else {
            return Vec::new();
        };
        let id = |part: &str| format!("grok:{offset}:{part}");
        match e.get("type").and_then(|v| v.as_str()) {
            Some("user") => {
                let text = e.get("content").map(text_of).unwrap_or_default();
                if text.is_empty() {
                    return Vec::new();
                }
                vec![Parsed::Turn(Turn {
                    id: id("user"),
                    at: None,
                    who: if e.get("synthetic_reason").and_then(|v| v.as_str()).is_some()
                        || is_harness_text(&text)
                    {
                        Who::Harness
                    } else {
                        Who::Person
                    },
                    body: Body::prose(text),
                })]
            }
            Some("assistant") => {
                let mut out = Vec::new();
                let text = e.get("content").map(text_of).unwrap_or_default();
                if !text.is_empty() {
                    out.push(Parsed::Turn(Turn {
                        id: id("assistant"),
                        at: None,
                        who: Who::Agent,
                        body: Body::prose(text),
                    }));
                }
                if let Some(calls) = e.get("tool_calls").and_then(|v| v.as_array()) {
                    for call in calls {
                        let Some(call_id) = call.get("id").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let name = call
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let input = call
                            .get("arguments")
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| v.to_string())
                            })
                            .unwrap_or_default();
                        let Told { title, detail } = told(&name, &input);
                        out.push(Parsed::Turn(Turn {
                            id: format!("grok-tool:{call_id}"),
                            at: None,
                            who: Who::Agent,
                            body: Body::Tool {
                                tasks: Vec::new(),
                                question: None,
                                name,
                                input,
                                title,
                                detail,
                                output: None,
                                images: Vec::new(),
                            },
                        }));
                    }
                }
                out
            }
            Some("tool_result") => {
                let Some(call_id) = e.get("tool_call_id").and_then(|v| v.as_str()) else {
                    return Vec::new();
                };
                vec![Parsed::ToolOutput {
                    id: format!("grok-tool:{call_id}"),
                    output: e.get("content").map(text_of).unwrap_or_default(),
                    images: e.get("content").map(images_in).unwrap_or_default(),
                }]
            }
            Some("reasoning") => {
                let text = e.get("summary").map(text_of).unwrap_or_default();
                if text.is_empty() {
                    return Vec::new();
                }
                vec![Parsed::Turn(Turn {
                    id: id("reasoning"),
                    at: None,
                    who: Who::Agent,
                    body: Body::Thinking { text },
                })]
            }
            _ => Vec::new(),
        }
    }
}

/// Where a bound session's transcript is, or `None` when there is
/// none to read. A recorded path (Claude's SessionStart says it)
/// wins; Claude without one falls back to the derivation the harness
/// uses; codex without one is found by its session id under its sessions dir;
/// Grok's path is derived from the working directory and session id.
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
            if let Some(path) = recorded {
                return Some(path.to_path_buf());
            }
            let mut found = Vec::new();
            codex_candidates(
                &home.join(".codex").join("sessions"),
                session_id,
                &mut found,
            );
            pick_newest(found)
        }
        Tool::Grok => Some(
            recorded
                .map(Path::to_path_buf)
                .unwrap_or_else(|| grok_derived(session_id, cwd, home)),
        ),
        _ => None,
    }
}

/// Grok stores one chat history per session beneath the percent-encoded
/// absolute working directory. The expected path is useful even before
/// the file exists: the tail can begin as soon as Grok first writes it.
fn grok_derived(session_id: &str, cwd: &Path, home: &Path) -> PathBuf {
    let mut encoded = String::new();
    for byte in cwd.to_string_lossy().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    home.join(".grok")
        .join("sessions")
        .join(encoded)
        .join(session_id)
        .join("chat_history.jsonl")
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
    /// The file read, by device and inode: a file renamed over the path is
    /// another record whatever its length.
    file: (u64, u64),
}

pub(crate) struct Polled {
    pub(crate) lines: Vec<TranscriptLine>,
    pub(crate) reset: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TranscriptLine {
    pub(crate) offset: u64,
    pub(crate) text: String,
}

impl Tail {
    /// Open at the file's end, returning the whole lines within the
    /// last `window` bytes — the files run to hundreds of megabytes
    /// and the page wants the last few screens, not the history.
    pub(crate) fn open(path: &Path, window: u64) -> std::io::Result<(Self, Vec<TranscriptLine>)> {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let len = meta.len();
        let mut start = len.saturating_sub(window);
        // Expand to the start of the boundary record. A single attachment
        // may exceed the lookback budget; dropping its head loses the whole
        // image on reconnect, or an in-progress record on its next append.
        let mut cursor = start;
        let mut chunk = [0u8; 8192];
        while cursor > 0 {
            let from = cursor.saturating_sub(chunk.len() as u64);
            let size = (cursor - from) as usize;
            file.seek(SeekFrom::Start(from))?;
            file.read_exact(&mut chunk[..size])?;
            if let Some(at) = chunk[..size].iter().rposition(|b| *b == b'\n') {
                start = from + at as u64 + 1;
                break;
            }
            cursor = from;
            start = from;
        }
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        // The cursor is where the read actually ended, not the length
        // sampled before it: bytes appended during the read would
        // otherwise be read twice (codex on 056a342).
        let pos = start + buf.len() as u64;
        let (lines, partial) = whole_lines(&buf, start);
        Ok((
            Self {
                path: path.to_path_buf(),
                pos,
                partial,
                window,
                file: (meta.dev(), meta.ino()),
            },
            lines,
        ))
    }

    pub(crate) fn poll(&mut self) -> std::io::Result<Polled> {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let mut file = std::fs::File::open(&self.path)?;
        let meta = file.metadata()?;
        let len = meta.len();
        if len < self.pos || (meta.dev(), meta.ino()) != self.file {
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
        let base = self.pos - self.partial.len() as u64;
        self.pos += fresh.len() as u64;
        let mut buf = std::mem::take(&mut self.partial);
        buf.extend_from_slice(&fresh);
        let (lines, rest) = whole_lines(&buf, base);
        self.partial = rest;
        Ok(Polled {
            lines,
            reset: false,
        })
    }
}

/// Complete lines, and the bytes after the last newline — a line
/// still being written is not parsed until it ends.
fn whole_lines(buf: &[u8], base: u64) -> (Vec<TranscriptLine>, Vec<u8>) {
    let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else {
        return (Vec::new(), buf.to_vec());
    };
    let mut offset = base;
    let lines = buf[..=last_nl]
        .split_inclusive(|b| *b == b'\n')
        .filter_map(|bytes| {
            let at = offset;
            offset += bytes.len() as u64;
            let text = String::from_utf8_lossy(bytes)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            (!text.trim().is_empty()).then_some(TranscriptLine { offset: at, text })
        })
        .collect();
    (lines, buf[last_nl + 1..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = include_str!("fixtures/claude.jsonl");
    const CODEX: &str = include_str!("fixtures/codex.jsonl");

    /// A call says what it is DOING, never the object it was called
    /// with: the row that draws it has one line, and
    /// `{"command":"for f in .clank/agents/…` spends it before the
    /// verb (a-tool-call-says-what-it-did).
    #[test]
    fn a_call_is_titled_by_what_it_is_for() {
        let told_by = |name: &str, input: &str| {
            let Told { title, detail } = told(name, input);
            (title, detail)
        };

        // The shape that started this: Claude writes the sentence.
        let (title, detail) = told_by(
            "Bash",
            r#"{"command":"for f in .clank/agents/*/feedback/*.md; do cat $f; done","description":"Read intro review feedback"}"#,
        );
        assert_eq!(title, "Read intro review feedback");
        assert!(
            detail.contains("for f in .clank/agents") && detail.contains("description:"),
            "the call, whole: {detail}"
        );

        // Without one, the command itself — as its own text, so a
        // multi-line command reads as the lines it is.
        let (title, detail) = told_by("Bash", "{\"command\":\"cargo test\\n  --all\"}");
        assert_eq!(title, "cargo test");
        assert_eq!(detail, "cargo test\n  --all", "no JSON punctuation");

        // The field that is the point of the call, per tool.
        assert_eq!(
            told_by("Read", r#"{"file_path":"/repo/src/lib.rs","limit":40}"#).0,
            "/repo/src/lib.rs"
        );
        assert_eq!(
            told_by("Grep", r#"{"pattern":"fn told","path":"x"}"#).0,
            "fn told"
        );
        assert_eq!(
            told_by(
                "WebFetch",
                r#"{"url":"https://example.com/x","prompt":"read it"}"#
            )
            .0,
            "https://example.com/x"
        );

        // codex sends its shell input as the command itself; a
        // function call sends the SAME thing as JSON in a string, and
        // both have to read as the command (codex on 3ad0dfc).
        assert_eq!(told_by("exec", "ls -la").0, "ls -la");
        assert_eq!(told_by("shell", r#"{"cmd":"ls"}"#).0, "ls");
        assert_eq!(told_by("shell", r#"{"cmd":["ls","-la"]}"#).0, "ls -la");
        // An argv's boundaries survive the expansion: these two ran
        // differently, and a detail that joined them would say the
        // same thing about both (codex on b3725e8).
        let spaced = told_by("shell", r#"{"cmd":["printf","%s","a b"]}"#).1;
        let split = told_by("shell", r#"{"cmd":["printf","%s","a","b"]}"#).1;
        assert_ne!(spaced, split, "two calls, two details");
        assert!(spaced.contains(r#""a b""#), "the argument, whole: {spaced}");

        // A tool nobody taught this still says something readable.
        assert_eq!(
            told_by("mcp__thing__do", r#"{"widget":"the blue one"}"#).0,
            "the blue one"
        );
        // A count is not a sentence. Metadata sorting before the
        // subject used to title the call `20` or `false`, which says
        // less than the tool's name does (codex on b3725e8).
        assert_eq!(
            told_by("mcp__thing__do", r#"{"count":3}"#).0,
            "mcp__thing__do"
        );
        assert_eq!(
            told_by("mcp_list", r#"{"limit":20,"repository":"llfourn/clank"}"#).0,
            "llfourn/clank",
            "the readable field, not the one that sorted first"
        );
        assert_eq!(
            told_by(
                "mcp_filter",
                r#"{"filter":{"status":"open"},"verbose":false}"#
            )
            .0,
            "mcp_filter"
        );
        assert!(
            told_by("mcp__thing__do", r#"{"count":3}"#)
                .1
                .contains("count: 3"),
            "and the number is in the detail, where it is worth reading"
        );
        assert_eq!(
            told_by("mcp__thing__do", "{}").0,
            "mcp__thing__do",
            "and with nothing to say, the tool's own name"
        );
        // A STRUCTURE is not a line: its JSON in the title is the
        // fault this is for, so the name is better (codex on b09ad51).
        let (title, detail) = told_by("mcp_filter", r#"{"filter":{"status":"open"}}"#);
        assert_eq!(title, "mcp_filter");
        assert!(
            detail.contains(r#"{"status":"open"}"#),
            "and the structure is in the detail, whole: {detail}"
        );

        // Nothing is dropped from a detail: a field the title could
        // not use is exactly what someone expands the call to read.
        let (_, detail) = told_by(
            "web_search",
            r#"{"search_query":[{"q":"rust docs"}],"response_length":"short"}"#,
        );
        assert!(
            detail.contains("rust docs") && detail.contains("response_length: short"),
            "every field, including the structured one: {detail}"
        );
        let (_, detail) = told_by("thing", r#"{"a":null,"b":"x"}"#);
        assert!(detail.contains("a: null"), "a null is a value: {detail}");

        // A title is a line's worth; the detail keeps the rest.
        let long = "x".repeat(400);
        let (title, detail) = told_by("Bash", &serde_json::json!({ "command": long }).to_string());
        assert!(
            title.chars().count() <= 121 && title.ends_with('…'),
            "{title}"
        );
        assert_eq!(detail.len(), 400, "the detail is not cut");
    }

    #[test]
    fn claude_question_calls_are_typed_and_keep_the_original_tool_input() {
        let input = serde_json::json!({"questions":[{"header":"Direction", "question":"Which way?",
            "multiSelect":true, "options":[{"label":"North","description":"First"},
                {"label":"South","description":"Second"}]}]});
        let line = serde_json::json!({"type":"assistant", "uuid":"assistant-turn", "message":{
            "role":"assistant", "content":[{"type":"tool_use","id":"question-call",
                "name":"AskUserQuestion", "input":input}]}})
        .to_string();
        let parsed = parse(Tool::Claude, &line);
        let t = turn(&parsed[0]);
        let Body::Tool {
            question: Some(question),
            input: raw,
            output,
            ..
        } = &t.body
        else {
            panic!("typed question")
        };
        assert_eq!(t.id, "question-call");
        assert_eq!(raw, &input.to_string());
        assert_eq!(question.fields[0].question, "Which way?");
        assert!(question.fields[0].multi_select);
        assert!(output.is_none());
        // Optional metadata does not invalidate older retained tool bodies.
        let old = serde_json::json!({"kind":"tool","name":"Bash","input":"ls",
            "output":null});
        let old: Body = serde_json::from_value(old).unwrap();
        assert!(matches!(
            old,
            Body::Tool { tasks, question: None, .. } if tasks.is_empty()
        ));
    }

    /// Both agents' fixtures, through the parser rather than the
    /// helper: neither draws raw JSON at a reader.
    #[test]
    fn neither_agents_calls_show_their_json() {
        for (who, tool, text) in [
            ("claude", Tool::Claude, CLAUDE),
            ("codex", Tool::Codex, CODEX),
        ] {
            let calls: Vec<(String, String)> = text
                .lines()
                .flat_map(|l| parse(tool, l))
                .filter_map(|p| match p {
                    Parsed::Turn(Turn {
                        body: Body::Tool { title, detail, .. },
                        ..
                    }) => Some((title, detail)),
                    _ => None,
                })
                .collect();
            assert!(!calls.is_empty(), "{who} has calls to show");
            for (title, detail) in &calls {
                assert!(!title.is_empty(), "{who}: a call with nothing to say");
                assert!(
                    !title.starts_with('{'),
                    "{who}: raw JSON as a title: {title}"
                );
                assert!(
                    !detail.starts_with('{'),
                    "{who}: raw JSON as a detail: {detail}"
                );
            }
        }
        // The fixture codex named: arguments are JSON inside a string.
        let shell = CODEX
            .lines()
            .flat_map(|l| parse(Tool::Codex, l))
            .find_map(|p| match p {
                Parsed::Turn(Turn {
                    body: Body::Tool { name, title, .. },
                    ..
                }) if name == "shell" => Some(title),
                _ => None,
            })
            .expect("the shell call");
        assert_eq!(shell, "ls", "the command, out of its JSON string");
    }

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
            "another harness's line is not a Grok turn"
        );
        assert!(!has_adapter(Tool::OpenCode) && has_adapter(Tool::Grok));
    }

    #[test]
    fn the_path_is_recorded_else_derived_or_found_by_id() {
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
        assert_eq!(
            transcript_path(
                Tool::Codex,
                "id1",
                Some(Path::new("/recorded/codex.jsonl")),
                cwd,
                home.path(),
            ),
            Some(PathBuf::from("/recorded/codex.jsonl"))
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
            Some(
                home.path()
                    .join(".grok/sessions/%2FUsers%2Flloyd%2Fsrc%2Fclank/x/chat_history.jsonl")
            )
        );
        assert_eq!(
            transcript_path(
                Tool::Grok,
                "x",
                Some(Path::new("/recorded/history.jsonl")),
                cwd,
                home.path()
            ),
            Some(PathBuf::from("/recorded/history.jsonl"))
        );
    }

    #[test]
    fn grok_chat_history_has_people_replies_tools_and_private_reasoning() {
        let user = r#"{"type":"user","content":[{"type":"text","text":"hello"}]}"#;
        let first = parse_at(Tool::Grok, user, 13);
        let second = parse_at(Tool::Grok, user, 91);
        assert_eq!(turn(&first[0]).id, "grok:13:user");
        assert_eq!(turn(&first[0]).who, Who::Person);
        assert_ne!(
            turn(&first[0]).id,
            turn(&second[0]).id,
            "identical prompts are distinct"
        );
        let synthetic = r#"{"type":"user","content":[{"type":"text","text":"wake"}],"synthetic_reason":"task_completed"}"#;
        assert_eq!(
            turn(&parse_at(Tool::Grok, synthetic, 120)[0]).who,
            Who::Harness
        );
        let assistant = r#"{"type":"assistant","content":"Done","tool_calls":[{"id":"call-1","name":"shell","arguments":"{\"command\":\"pwd\"}"},{"id":"call-2","name":"Read","arguments":"{\"file_path\":\"a.rs\"}"}]}"#;
        let parsed = parse_at(Tool::Grok, assistant, 200);
        assert_eq!(parsed.len(), 3);
        assert_eq!(turn(&parsed[0]).id, "grok:200:assistant");
        assert_eq!(turn(&parsed[1]).id, "grok-tool:call-1");
        assert_eq!(turn(&parsed[2]).id, "grok-tool:call-2");
        let result = r#"{"type":"tool_result","tool_call_id":"call-2","content":"file text"}"#;
        assert!(
            matches!(&parse_at(Tool::Grok, result, 400)[0], Parsed::ToolOutput { id, output, .. } if id == "grok-tool:call-2" && output == "file text")
        );
        let reasoning = r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"Checking the file"}],"encrypted_content":"secret"}"#;
        assert!(
            matches!(&turn(&parse_at(Tool::Grok, reasoning, 500)[0]).body, Body::Thinking { text } if text == "Checking the file")
        );
        assert!(
            parse_at(
                Tool::Grok,
                r#"{"type":"reasoning","encrypted_content":"secret"}"#,
                600
            )
            .is_empty()
        );
    }

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[test]
    fn codex_attachment_wrappers_are_structural_and_keep_order_and_identity() {
        // Shape observed in this session's response_item user record. Local
        // path text is metadata, never permission to read a file.
        let opening = serde_json::json!({"type":"input_text","text":"<image name=[Image #1] path=\"/var/private/screenshot.png\">"});
        let image = serde_json::json!({"type":"input_image","image_url":format!("data:image/png;base64,{PNG}")});
        let closing = serde_json::json!({"type":"input_text","text":"</image>"});
        let prose = |text| serde_json::json!({"type":"input_text","text":text});
        let record = |content| {
            serde_json::json!({"type":"response_item","payload":{"type":"message","id":"user-images","role":"user","content":content}}).to_string()
        };
        let content = serde_json::json!([prose("before"),opening,image,closing,prose("between"),
            opening,{"type":"input_image","image_url":"file:///must-not-read.png"},closing,prose("after")]);
        let line = record(content);
        let got = parse_at(Tool::Codex, &line, 100);
        assert_eq!(got, parse_at(Tool::Codex, &line, 100), "replay is stable");
        assert_eq!(got.len(), 5);
        assert!(
            matches!(&turn(&got[1]).body, Body::Image { image } if png(image) && image.label == "Image #1")
        );
        assert!(turn(&got[1]).id.ends_with(":2"));
        assert!(matches!(&turn(&got[3]).body, Body::Image { image } if image.data.is_none()));
        for (at, expected) in [(0, "before"), (2, "between"), (4, "after")] {
            assert!(matches!(&turn(&got[at]).body, Body::Text { text,.. } if text == expected));
        }
        let literal = parse(
            Tool::Codex,
            &record(serde_json::json!([
                opening,
                prose("literal markup"),
                closing
            ])),
        );
        assert_eq!(
            literal.len(),
            3,
            "text resembling a wrapper alone stays visible"
        );
        assert!(
            matches!(&turn(&literal[0]).body,Body::Text{text,..} if text.starts_with("<image"))
        );
    }

    #[test]
    fn a_boundary_record_larger_than_the_lookback_survives_reconnect_and_append() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chat.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "old record").unwrap();
        let record = format!("image:{}", "x".repeat(4096));
        write!(file, "{record}").unwrap();
        file.flush().unwrap();
        let (mut tail, lines) = Tail::open(&path, 100).unwrap();
        assert!(lines.is_empty());
        writeln!(file).unwrap();
        file.flush().unwrap();
        let completed = tail.poll().unwrap();
        assert_eq!(completed.lines[0].text, record);
        assert_eq!(completed.lines[0].offset, 11);
        assert!(tail.poll().unwrap().lines.is_empty());
        let (_, replay) = Tail::open(&path, 100).unwrap();
        assert_eq!(replay, completed.lines);
    }

    fn png(image: &Image) -> bool {
        image.media_type == "image/png" && image.data.as_deref() == Some(PNG) && image.bytes == 70
    }

    /// A pasted picture is a turn of the person's, beside the text it
    /// came with — which is the person's too, not a harness note, list
    /// or no list; a picture in a tool's result rides on the output;
    /// originals are retained, while snapshots ship only descriptors.
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

        let big = "A".repeat(3 * 1024 * 1024);
        let got = parse(
            Tool::Claude,
            &line(
                serde_json::json!([{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":big.clone()}}]),
            ),
        );
        match &turn(&got[0]).body {
            Body::Image { image } => {
                assert_eq!(
                    image.data.as_deref(),
                    Some(big.as_str()),
                    "original retained without a size cutoff"
                );
                assert!(image.bytes > 2 * 1024 * 1024);
                assert!(
                    serde_json::to_value(image).unwrap().get("data").is_none(),
                    "snapshots contain descriptors, not pixels"
                );
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
            "a missing image keeps one attachment placeholder"
        );
        assert_eq!(turn(&got[0]).id, "m7:0");
        assert!(matches!(&turn(&got[0]).body, Body::Image { image } if image.data.is_none()));

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
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec![r#"{"n":1}"#],
            "only the whole line at open"
        );
        assert_eq!(lines[0].offset, 0);
        write!(f, r#" of it"}}"#).unwrap();
        writeln!(f).unwrap();
        f.flush().unwrap();
        let p = tail.poll().unwrap();
        assert_eq!(
            p.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec![r#"{"n":2,"text":"half of it"}"#],
            "the record, once, whole"
        );
        assert_eq!(p.lines[0].offset, r#"{"n":1}"#.len() as u64 + 1);
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
            lines[0].text.starts_with('{') && lines[0].text.ends_with('}'),
            "no half line: {}",
            lines[0].text
        );
        assert!(lines.last().unwrap().text.contains("\"n\":49"));

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
        assert_eq!(
            p.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec![r#"{"n":50}"#, r#"{"n":51}"#]
        );
        assert_eq!(
            p.lines[0].offset,
            std::fs::metadata(&path).unwrap().len()
                - p.lines.iter().map(|l| l.text.len() as u64 + 1).sum::<u64>()
        );
        assert!(!p.reset);

        std::fs::write(&path, "{\"n\":0}\n").unwrap();
        let p = tail.poll().unwrap();
        assert!(p.reset, "a shrink is a new generation");
        assert_eq!(
            p.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec![r#"{"n":0}"#]
        );
        assert_eq!(p.lines[0].offset, 0);
    }
}
