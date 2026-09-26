//! Typed shapes for the Stop-hook stdin/stdout protocol.
//!
//! Both claude and codex pass JSON on the hook's stdin and read
//! the hook's exit code + stdout/stderr to decide whether to
//! continue the agent. The wire shapes look like:
//!
//! - claude stdin: `{ session_id, transcript_path, cwd,
//!   stop_hook_active, last_assistant_message, ... }` plus
//!   claude-only extras (`effort`, `background_tasks`, ...).
//! - codex stdin: same common fields plus codex-only extras
//!   (`turn_id`, `model`, ...).
//!
//! [`HookInput`] captures just the common fields the stop-hook
//! adapter actually uses; unknown fields are silently ignored
//! (`#[serde(default)]` + a deny-list of zero extras).
//!
//! [`HookOutcome`] is what the stop-hook adapter decides; the
//! per-tool wire form is the CLI's job to render — see the
//! `claude_*` / `codex_*` helpers and the [`CodexBlockDecision`]
//! struct (which serializes to codex's `{"decision":"block",
//! "reason":"..."}` stdout shape).
//!
//! Per-tool exit-code contract (from the plan):
//!
//! | Outcome             | Claude                  | Codex                                          |
//! | ---                 | ---                     | ---                                            |
//! | `Continue { reason }` | exit 2, stderr=reason | exit 0, stdout=`{decision:"block",reason:...}` |
//! | `Silent`            | exit 0, no output       | exit 0, no output                              |
//! | `Diagnostic { msg }` | exit 0, stderr=msg     | exit 0, stderr=msg                             |

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;
use crate::vocab::Tool;

/// Common fields the stop-hook adapter reads from hook stdin.
/// Both tools include all of these.
///
/// `#[serde(default)]` on optional fields and ignoring unknown
/// extras (claude's `effort` / `background_tasks` / `session_crons`,
/// codex's `turn_id` / `model`) keeps the type forward-compatible
/// with either agent adding fields later.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HookInput {
    pub session_id: SessionId,
    pub cwd: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// True iff this hook invocation is itself a continuation of a
    /// previous Stop-hook decision. The adapter does not act on
    /// this — claude's own 8-block cap (which resets on tool use
    /// between fires) is the safety net for runaway loops.
    pub stop_hook_active: bool,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    /// Claude's in-flight background work at turn end:
    /// `run_in_background` shells, subagents, MCP monitors, … . Claude
    /// lists ONLY live tasks here (`[]` once they finish) and re-fires
    /// Stop when one completes — that's the documented signal for "the
    /// turn ended because the session is *paused waiting for background
    /// work to wake it back up*," not because it's idle. See
    /// [`background_disposition`]. Codex omits the field, so it stays empty
    /// there.
    #[serde(default)]
    pub background_tasks: Vec<BackgroundTask>,
}

/// One entry of claude's Stop-hook `background_tasks` array. `command`
/// (the raw shell command line for `shell` tasks) is captured so the hook
/// can recognize the agent's own backgrounded `clank wait`; the rest
/// (`description`, `type`, …) are ignored. Forward-compatible: every field
/// is optional.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BackgroundTask {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
}

impl BackgroundTask {
    /// True if this task is the agent's own `clank wait` long-poll.
    /// Best-effort: one shell command segment's program basename is `clank`
    /// and its first arg is `wait`. Common shell wrappers and prefixes are
    /// unpicked without executing the command or attempting a full shell
    /// parse.
    pub fn is_clank_wait(&self) -> bool {
        let Some(cmd) = self.command.as_deref() else {
            return false;
        };
        command_contains_clank_wait(cmd, 0)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ShellToken {
    Word(String),
    Boundary,
}

fn command_contains_clank_wait(command: &str, depth: usize) -> bool {
    const MAX_WRAPPER_DEPTH: usize = 8;
    if depth > MAX_WRAPPER_DEPTH {
        return false;
    }

    let tokens = shell_tokens(command);
    tokens
        .split(|token| matches!(token, ShellToken::Boundary))
        .any(|segment| {
            let words: Vec<&str> = segment
                .iter()
                .filter_map(|token| match token {
                    ShellToken::Word(word) => Some(word.as_str()),
                    ShellToken::Boundary => None,
                })
                .collect();
            segment_contains_clank_wait(&words, depth)
        })
}

/// Index of the actual program word, skipping leading `VAR=value`
/// assignments and `env` / `exec` / `command` wrappers.
///
/// Shared by every command recogniser here so the wrapper rules cannot
/// drift apart between them.
fn command_word_index(words: &[&str]) -> usize {
    let mut command = 0;
    loop {
        while words.get(command).is_some_and(|word| is_assignment(word)) {
            command += 1;
        }
        match words.get(command).map(|word| program_basename(word)) {
            Some("env" | "exec" | "command") => {
                command += 1;
                if words.get(command) == Some(&"--") {
                    command += 1;
                }
            }
            _ => return command,
        }
    }
}

fn segment_contains_clank_wait(words: &[&str], depth: usize) -> bool {
    let command = command_word_index(words);
    let Some(program) = words.get(command).map(|word| program_basename(word)) else {
        return false;
    };
    if program == "clank" && words.get(command + 1) == Some(&"wait") {
        return true;
    }

    program.ends_with("sh")
        && words.get(command + 1) == Some(&"-lc")
        && words
            .get(command + 2)
            .is_some_and(|script| command_contains_clank_wait(script, depth + 1))
}

/// The `--desc` VALUE of a `clank run` invocation, or `None` if this
/// command line is not one.
///
/// Structured recognition, not substring matching: `echo "test run"`
/// names the same words and is not a run, and a `clank run` whose
/// description happens to appear later on its own line is not a match
/// for that later text either. The description is the only handle the
/// hook has on an owned run — the tool assigns a task id only after
/// launching the command, so no id can exist in the marker — which is
/// exactly why the handle has to be recognised precisely.
///
/// Stops at `--`: everything after it belongs to the command being
/// run, so a `--desc` there is the CHILD's flag, not clank's.
pub fn clank_run_desc(command: &str) -> Option<String> {
    let tokens = shell_tokens(command);
    tokens
        .split(|token| matches!(token, ShellToken::Boundary))
        .find_map(|segment| {
            let words: Vec<&str> = segment
                .iter()
                .filter_map(|token| match token {
                    ShellToken::Word(word) => Some(word.as_str()),
                    ShellToken::Boundary => None,
                })
                .collect();
            let at = command_word_index(&words);
            if words.get(at).map(|w| program_basename(w)) != Some("clank")
                || words.get(at + 1) != Some(&"run")
            {
                return None;
            }
            let mut i = at + 2;
            while let Some(word) = words.get(i) {
                if *word == "--" {
                    return None;
                }
                if *word == "--desc" {
                    return words.get(i + 1).map(|v| v.trim().to_string());
                }
                if let Some(v) = word.strip_prefix("--desc=") {
                    return Some(v.trim().to_string());
                }
                i += 1;
            }
            None
        })
        .filter(|d| !d.is_empty())
}

/// One operator-faithful token of a launch command.
///
/// Separate from [`shell_tokens`], which flattens every operator to a
/// generic `Boundary`. That flattening is right for "is there a wait
/// in here?" and wrong for "does this task's completion mean the wait
/// finished?", because `&&` and `&` become the same thing.
#[derive(Debug, PartialEq, Eq)]
enum LaunchTok {
    Word(String),
    /// `;`, newline, `&&`, `||` — the shell WAITS for what precedes.
    Sequence,
    /// Anything that detaches, nests, or that this grammar does not
    /// model: `&`, `|`, `(`, `)`, `<(`, `$(`, a backtick.
    Unsupported,
}

/// The `--tag` VALUE of a `clank wait` whose COMPLETION is still the
/// listed task's completion, or `None`.
///
/// ONE conservative grammar, syntax-aware. Two earlier versions each
/// got a piece of this wrong, and the second failed because it used
/// TWO views that disagreed (codex 8fac98f):
///
/// - a substring search accepted `--tag Tff`, a token in a comment,
///   and a token in another command's arguments;
/// - a raw `&` scan beside flattened segments accepted
///   `NOTE=issue#123 clank wait --tag T &` — the `#` is inside an
///   assignment VALUE, so the scan read the rest of the line as a
///   comment and never saw the `&`, while the other view read the
///   assignment correctly and found the wait.
///
/// And no `&` is needed to detach at all: `true <(clank wait --tag T)`
/// launches the wait asynchronously, and a parser that turns `(` into
/// a generic boundary sees a foreground command.
///
/// So the supported grammar is small and everything else is refused:
/// simple commands joined by `;`, newline, `&&` or `||`, one of which
/// is a `clank wait` carrying a non-empty `--tag`. Redirections are
/// fine — they do not detach. Backgrounding, pipelines, subshells and
/// substitutions are `None`, which reads as `Unknown`, which asks for
/// a wait it can name. Refusing a launch form clank never asks for
/// costs a nudge; accepting one costs an agent.
pub fn registered_wait_tag(command: &str) -> Option<String> {
    let tokens = launch_tokens(command);
    if tokens.iter().any(|t| *t == LaunchTok::Unsupported) {
        return None;
    }
    // EVERY segment must be a modelled simple command, and the whole
    // recognition fails if one is not. Skipping a segment that does
    // not match and searching on is what let a body be read as a
    // command: with a newline, `coproc if true; then` / `exec clank
    // wait --tag T` / `fi` puts the wait in its own segment, whose
    // program really is `clank` once `exec` is skipped (codex
    // 98579b8).
    let mut found: Option<String> = None;
    for segment in tokens.split(|t| *t == LaunchTok::Sequence) {
        let words: Vec<&str> = segment
            .iter()
            .filter_map(|t| match t {
                LaunchTok::Word(w) => Some(w.as_str()),
                _ => None,
            })
            .collect();
        if words.is_empty() {
            continue; // blank between separators
        }
        if !words.iter().all(|w| modelled(w)) {
            return None;
        }
        let at = command_word_index(&words);
        let Some(program) = words.get(at).map(|w| program_basename(w)) else {
            return None;
        };
        // A KEYWORD in program position means this segment is part of
        // a construct, not a command — and the construct decides what
        // the segments around it mean.
        if SHELL_KEYWORDS.contains(&program) {
            return None;
        }
        if program != "clank" || words.get(at + 1) != Some(&"wait") {
            continue; // an ordinary prefix command, e.g. `cd /repo`
        }
        if found.is_some() {
            return None; // two waits in one launch: which is it?
        }
        found = tag_of(&words[at + 2..]);
        if found.is_none() {
            return None; // a `clank wait` carrying no registration
        }
    }
    found.filter(|tag| !tag.is_empty())
}

/// The `--tag` value among a `clank wait`'s arguments.
fn tag_of(args: &[&str]) -> Option<String> {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        // Everything past `--` belongs to something else.
        if *word == "--" {
            return None;
        }
        if *word == "--tag" {
            return args.get(i + 1).map(|v| v.trim().to_string());
        }
        if let Some(v) = word.strip_prefix("--tag=") {
            return Some(v.trim().to_string());
        }
        i += 1;
    }
    None
}

/// Words that introduce a shell construct rather than run a program.
///
/// Checked in PROGRAM position of every segment, and a hit fails the
/// whole recognition. An earlier version dropped this list after a
/// mutation survived, on the reasoning that a keyword always lands in
/// program position and the program must be `clank` — true for
/// one-line forms and false the moment the body starts on the next
/// line, which is exactly what the mutation was telling me and I
/// misread as unreachable code (codex 98579b8).
const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "coproc", "time", "!",
];

/// Whether one word belongs to the modelled launch form.
///
/// A plain program name, argument, or `NAME=value` assignment — and
/// nothing carrying shell syntax. `<<` (a here-document), `>out` (a
/// redirection), `$X` (an expansion) and `{` (a group) are all
/// refused, because each changes what the surrounding text MEANS and
/// this grammar reasons only about the words it can read literally.
///
/// Narrow on purpose: clank's arm instruction asks for
/// `clank wait --tag <token>`, so the forms worth accepting are that
/// one and the sequencing an agent naturally puts in front of it.
/// Anything else is `Unknown`, which asks again rather than sleeping.
///
/// This is only half the check — see [`SHELL_KEYWORDS`] for the other
/// half, which is what a segment's program word may be.
fn modelled(word: &str) -> bool {
    !word.is_empty() && !word.contains(['<', '>', '&', '|', '{', '}', '$', '`', '(', ')', ';'])
}

fn launch_tokens(command: &str) -> Vec<LaunchTok> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = command.chars().peekable();
    let mut flush = |out: &mut Vec<LaunchTok>, word: &mut String, started: &mut bool| {
        if *started {
            out.push(LaunchTok::Word(std::mem::take(word)));
            *started = false;
        }
    };

    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                started = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                started = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(e) = chars.next() {
                                word.push(e);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(e) = chars.next() {
                    word.push(e);
                }
            }
            // A comment only where a WORD could start. Inside
            // `NOTE=issue#123` the `#` is data, and reading it as a
            // comment is what hid a trailing `&`.
            '#' if !started => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                flush(&mut out, &mut word, &mut started);
                out.push(LaunchTok::Sequence);
            }
            ';' | '\n' => {
                flush(&mut out, &mut word, &mut started);
                out.push(LaunchTok::Sequence);
            }
            '&' | '|' => {
                flush(&mut out, &mut word, &mut started);
                if chars.peek() == Some(&ch) {
                    chars.next();
                    // `&&` / `||`: the shell waits for what precedes.
                    out.push(LaunchTok::Sequence);
                } else {
                    // A lone `&` detaches; a lone `|` builds a
                    // pipeline this grammar does not model.
                    out.push(LaunchTok::Unsupported);
                }
            }
            // A PARENTHESIS ANYWHERE, and a backtick. One rule, not
            // three: a subshell `(…)`, a process substitution `<(…)`
            // and a command substitution `$(…)` all contain the
            // paren, so matching it wherever it appears covers each —
            // and separate arms for `<(` and `$(` were unreachable
            // behind it, which a mutation showed by surviving their
            // removal.
            //
            // Process substitution is why this cannot be a boundary:
            // `true <(clank wait --tag T)` launches the wait
            // ASYNCHRONOUSLY with no `&` anywhere, and a parser that
            // flattens `(` sees a foreground command (codex 8fac98f).
            '(' | ')' | '`' => {
                flush(&mut out, &mut word, &mut started);
                out.push(LaunchTok::Unsupported);
            }
            c if c.is_whitespace() => flush(&mut out, &mut word, &mut started),
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    flush(&mut out, &mut word, &mut started);
    out
}

fn program_basename(word: &str) -> &str {
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some('_' | 'a'..='z' | 'A'..='Z'))
        && chars.all(|ch| matches!(ch, '_' | 'a'..='z' | 'A'..='Z' | '0'..='9'))
}

fn shell_tokens(command: &str) -> Vec<ShellToken> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                word_started = true;
                for quoted in chars.by_ref() {
                    if quoted == '\'' {
                        break;
                    }
                    word.push(quoted);
                }
            }
            '"' => {
                word_started = true;
                while let Some(quoted) = chars.next() {
                    match quoted {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        _ => word.push(quoted),
                    }
                }
            }
            '\\' => {
                word_started = true;
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            '#' if !word_started => {
                for comment in chars.by_ref() {
                    if comment == '\n' {
                        push_boundary(&mut tokens, &mut word, &mut word_started);
                        break;
                    }
                }
            }
            '\n' => push_boundary(&mut tokens, &mut word, &mut word_started),
            ch if ch.is_whitespace() => push_word(&mut tokens, &mut word, &mut word_started),
            ';' | '(' | ')' => push_boundary(&mut tokens, &mut word, &mut word_started),
            '&' | '|' => {
                push_boundary(&mut tokens, &mut word, &mut word_started);
                if chars.peek() == Some(&ch) {
                    chars.next();
                }
            }
            _ => {
                word_started = true;
                word.push(ch);
            }
        }
    }
    push_word(&mut tokens, &mut word, &mut word_started);
    tokens
}

fn push_word(tokens: &mut Vec<ShellToken>, word: &mut String, word_started: &mut bool) {
    if *word_started {
        tokens.push(ShellToken::Word(std::mem::take(word)));
        *word_started = false;
    }
}

fn push_boundary(tokens: &mut Vec<ShellToken>, word: &mut String, word_started: &mut bool) {
    push_word(tokens, word, word_started);
    if !matches!(tokens.last(), Some(ShellToken::Boundary)) {
        tokens.push(ShellToken::Boundary);
    }
}

impl HookInput {
    /// How live a backgrounded `clank wait` is, on the evidence the
    /// harness provides.
    ///
    /// **`Live` requires a REGISTRATION.** A [`BackgroundTask`]
    /// carries `id`, `status` and `command` — no pid and no process
    /// identity — so nothing here ties a listed task to a running
    /// process on its own. Three attempts at inferring that tie
    /// failed for the same reason, each looking like proof and
    /// proving something else (codex 07c7d8d):
    ///
    /// - the LIST alone: its own doc, above, records that it names
    ///   tasks which have already finished;
    /// - a live pid: the OS reuses numbers, so it can be a stranger;
    /// - an inherited session variable: every descendant of a tool
    ///   shell has it, including a detached wait with its output
    ///   redirected to a file, whose completion notifies nobody.
    ///
    /// So the tie is CONSTRUCTED rather than inferred: the hook's arm
    /// instruction carries a fresh token, the agent runs it, and the
    /// wait publishes the same token in its claim. One token in both
    /// places names one process.
    ///
    /// Without it the answer is `Dead` when the harness says the task
    /// finished — that it does know — and `Unknown` otherwise. **And
    /// `Unknown` does not suppress.** Yielding to an unproven waiter
    /// is the safepay failure: a completed `clank wait` still in the
    /// list silenced every turn-end, permanently, because nothing
    /// re-checked (the-turn-end-is-one-decision).
    pub fn background_wait_liveness(&self, registered: Option<&str>) -> Liveness {
        let live: Vec<&BackgroundTask> = self
            .background_tasks
            .iter()
            .filter(|t| t.is_clank_wait() && t.status.as_deref() != Some("completed"))
            .collect();
        if live.is_empty() {
            return Liveness::Dead;
        }
        // The REGISTRATION: a token that is in the command the agent
        // ran and in the claim the wait published. Exactly one live
        // listed task may carry it — two would make "which process is
        // this?" a guess again, which is the question the token
        // exists to answer.
        let Some(tag) = registered.filter(|t| !t.is_empty()) else {
            return Liveness::Unknown;
        };
        let mut tagged = live.iter().filter(|t| {
            t.command
                .as_deref()
                .and_then(registered_wait_tag)
                .is_some_and(|found| found == tag)
        });
        match (tagged.next(), tagged.next()) {
            (Some(_), None) => Liveness::Live,
            _ => Liveness::Unknown,
        }
    }

    /// True if any in-flight background task is NOT a `clank wait` — a
    /// process the agent is parked on. Claude Code wakes the session when it
    /// completes, so the hook must not block in a wait while it runs.
    pub fn has_non_wait_background_work(&self) -> bool {
        self.background_tasks.iter().any(|t| !t.is_clank_wait())
    }

    /// The LIVE background tasks that are a `clank run` carrying this
    /// description.
    ///
    /// The caller wants exactly one. Zero proves nothing is running.
    /// Two or more means the description cannot tell them apart, and
    /// picking either would be a guess about whether an agent may be
    /// silenced — so both cases must decline to suppress. The tasks
    /// themselves are returned, not a count, because the unique match
    /// carries the one thing a `clank run` marker cannot: the harness
    /// task id the agent would need to re-attend it.
    pub fn live_clank_runs(&self, desc: &str) -> Vec<&BackgroundTask> {
        self.background_tasks
            .iter()
            .filter(|t| t.status.as_deref() != Some("completed"))
            .filter(|t| {
                t.command
                    .as_deref()
                    .is_some_and(|cmd| clank_run_desc(cmd).as_deref() == Some(desc))
            })
            .collect()
    }
}

/// How the Stop hook should treat the turn's background work — a pure
/// function of the turn (no clank identity/auto-mode), so the yield
/// cases can be decided before any repo/identity resolution. See
/// [`background_disposition`].
///
/// The two yields are separate variants because only one of them has
/// a waker. Collapsing them lost the proof at its only source: the
/// turn-end classifier downstream then had a bare label to work from
/// and named a registered wait where none existed
/// (the-turn-end-is-one-decision).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BgDisposition {
    /// A registered `clank wait` was proven live, and this is the
    /// token that proved it. Yield to it.
    YieldToWait { tag: String },
    /// Background work is live, and this tool has no completion wake
    /// to deliver when it ends.
    ///
    /// NOT a yield, and the name is load-bearing: called one, it
    /// invited the silent turn-end it used to produce — the agent left
    /// with nothing watching, and the hook's own wait, its only
    /// continuation, skipped to get there (grok 36518be9bfd9).
    LiveWorkNoWake,
    /// A process is live (claude) with no `clank wait` watching. The cli
    /// must peek for immediate work to tell the two cases apart: the agent
    /// still has work → hand it the ready items directly; the agent
    /// is idle (e.g. just committed, awaiting reviews) → nudge it to
    /// start `clank wait` alongside the process.
    NeedsWorkCheck,
    /// No background work — proceed to the normal deliver-work / idle wait.
    NoBackgroundWork,
}

/// Pure background-work disposition for the Stop hook. Claude's
/// `background_tasks` drives it; codex omits the field, so this is always
/// [`BgDisposition::NoBackgroundWork`] there. Note there is NO
/// `stop_hook_active` dependence: re-nudging is prevented structurally
/// (once a `clank wait` is armed it shows up here as a yield to it), and the
/// nudge fires only when the agent has no immediate work — precisely when a
/// backgrounded `clank wait` would block and persist.
impl HookInput {
    /// Ids of background tasks the tool reports as live this turn.
    ///
    /// ONE of the three signals an `attending` marker needs, and not
    /// sufficient alone: this list has been observed naming tasks that
    /// had already finished (`a-dead-pid-voids-a-falsely-live-marker`
    /// records a `cargo install` that exited 0, its pid gone from the
    /// process table, still listed by the very next hook). Suppression
    /// also requires a live pid and a token that still holds it.
    pub fn live_task_ids(&self) -> Vec<String> {
        self.background_tasks
            .iter()
            .filter(|t| t.status.as_deref() != Some("completed"))
            .filter_map(|t| t.id.clone())
            .collect()
    }
}

/// How live a waker is, on the evidence in hand.
///
/// Three answers, not two. `Unknown` is the one that matters: it is
/// what the harness task list can support on its own, and treating it
/// as `Live` is how an agent goes to sleep for good
/// (the-turn-end-is-one-decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Dead,
    Unknown,
}

/// `registered` is the token from a claim that already passed birth,
/// session and incarnation — see [`HookInput::background_wait_liveness`].
pub fn background_disposition(
    tool: Tool,
    input: &HookInput,
    registered: Option<&str>,
) -> BgDisposition {
    // Only a REGISTERED live wait takes the turn. `Unknown` falls
    // through to the paths below, which arm or nudge
    // (the-turn-end-is-one-decision).
    if input.background_wait_liveness(registered) == Liveness::Live {
        // Live implies a registration matched, so the token exists.
        return BgDisposition::YieldToWait {
            tag: registered.unwrap_or_default().to_string(),
        };
    }
    if input.has_non_wait_background_work() {
        if tool == Tool::Claude {
            return BgDisposition::NeedsWorkCheck;
        }
        return BgDisposition::LiveWorkNoWake;
    }
    BgDisposition::NoBackgroundWork
}

/// What the stop-hook adapter decides to emit after evaluating
/// agent config + work state. CLI maps to per-tool wire output
/// (see [`claude_continuation_exit_code`],
/// [`codex_continuation_stdout`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Resume the agent with `reason` as a continuation prompt.
    /// The agent runs another turn against this text.
    Continue { reason: String },
    /// The OpenCode plugin must start another long-poll after this
    /// one's ceiling. This is a plugin control action, not a model
    /// prompt; it keeps the wait alive without an idle-turn loop.
    RenewWait,
    /// Let the agent stop normally. No output to either tool. `why`
    /// carries WHICH silent branch fired — never on the wire, but
    /// first-class so "the hook chose not to wait" stays
    /// distinguishable per-branch (and testable) instead of one
    /// indistinct silence.
    Silent { why: SilentReason },
    /// Internal problem (config parse, identity unresolvable,
    /// projection failure). The hook NEVER fails the agent —
    /// the diagnostic goes to stderr and exit is still 0 so the
    /// turn ends cleanly. Clank bugs show up here; production
    /// failures don't compound by also breaking the agent.
    Diagnostic { message: String },
}

/// One declaration per outcome: enum, wire token and `ALL` together,
/// for the same reason `silent_reasons!` does it — a second
/// handwritten list is what this plan keeps deleting.
macro_rules! turns_left {
    ($($(#[$doc:meta])* $variant:ident => $tag:literal),+ $(,)?) => {
        /// What a turn END left behind, recorded beside the stamp.
        ///
        /// A stamp says a turn ended; it cannot say whether the agent
        /// was handed a next step or left with nothing, and those look
        /// identical from outside hours later. That difference is what
        /// separates a healthy agent mid-turn from one asleep
        /// (the-turn-end-is-one-decision).
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum TurnLeft {
            $($(#[$doc])* $variant),+
        }

        impl TurnLeft {
            /// The token written to `turn.end`.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(TurnLeft::$variant => $tag),+
                }
            }

            /// `None` for a token this build has no arm for — a stamp
            /// from a newer binary must not make a reader guess.
            pub fn parse(s: &str) -> Option<Self> {
                Some(match s {
                    $($tag => TurnLeft::$variant,)+
                    _ => return None,
                })
            }

            /// Every outcome. Generated, so it cannot omit one.
            pub const ALL: &'static [TurnLeft] = &[$(TurnLeft::$variant),+];
        }
    };
}

turns_left! {
    /// The agent was handed text it acts on. It has a next step, so
    /// a later reader must not call it asleep for lacking a waker.
    Armed => "armed",
    /// The turn yielded to a named waker that was live AT THE TIME.
    /// Whether it still is, is the reader's question — and the one
    /// that catches an agent whose waker died.
    Yielded => "yielded",
    /// The hook itself kept working and is still holding the turn —
    /// only written by a reader that catches a hook mid-flight, never
    /// as a terminal verdict.
    HookHeld => "hook_held",
    /// An intentional stop: auto off, superseded, cancelled.
    NotOurs => "not_ours",
    /// Clank could not decide, and said so. Nothing was armed.
    Failed => "failed",
    /// A turn from an older build ended with no continuation at all.
    /// Retained so old `turn.end` stamps remain readable.
    Nothing => "nothing",
}

/// One declaration per reason: the enum and [`SilentReason::ALL`] are
/// generated from it together.
///
/// A second handwritten list is exactly what milestone 1 deleted, and
/// adding one back here was not caught by a test that MATCHED on the
/// list rather than on the enum: a new variant plus its match arm,
/// with `ALL` left alone, passed (codex cfb98cbc83a9).
macro_rules! silent_reasons {
    ($($(#[$doc:meta])* $variant:ident),+ $(,)?) => {
        /// Why the hook let the agent stop silently — first-class data
        /// so each branch is named rather than re-derived at the call
        /// sites.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum SilentReason {
            $($(#[$doc])* $variant),+
        }

        impl SilentReason {
            /// Every reason. Generated, so it cannot omit one — which
            /// is what lets a caller quantify over the whole space
            /// instead of a hand-picked subset.
            pub const ALL: &'static [SilentReason] = &[$(SilentReason::$variant),+];
        }
    };
}

silent_reasons! {
    /// A REGISTERED `clank wait` was proven live this turn; yield to
    /// it. The only yield there is: the unproven one was a silence
    /// with a better name, and the tools that produced it now wait
    /// in-hook instead (the-turn-end-is-one-decision).
    YieldArmed,
    /// Effective auto-mode is off.
    AutoOff,
    /// `clank attending --clear` removed the check-in this turn was
    /// holding. The attendance was cancelled on purpose, so nothing
    /// watches and nothing should.
    AttendanceCleared,
    /// A live same-generation waiter already holds this session's
    /// park (claude-asyncrewake-work-loop); one waiter per session.
    WaiterAlreadyParked,
    /// This waiter's generation went stale (a newer session
    /// incarnation took over); its wake is suppressed.
    StaleGeneration,
    /// Every item was work the agent is already attending: it has a
    /// LIVE background task recorded, and nothing arrived that it did
    /// not already know. Waking here is what turned one long build
    /// into a dozen identical wakes
    /// (attending-suppresses-standing-wakes).
    AttendingBackgroundTask,
}

/// Codex's stop-hook continuation wire shape, written to stdout
/// when the adapter wants to resume codex with a prompt.
///
/// Codex reads `decision="block"` as "do not stop, continue with
/// `reason` as the next user prompt." (`decision` is always
/// `"block"` for our use; codex supports other values that don't
/// apply here.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexBlockDecision<'a> {
    pub decision: &'static str,
    pub reason: &'a str,
}

impl<'a> CodexBlockDecision<'a> {
    pub fn from_reason(reason: &'a str) -> Self {
        Self {
            decision: "block",
            reason,
        }
    }
}

/// Exit code claude wants for a continuation. (`2` per
/// claude's hook protocol — the agent reads stderr as the
/// continuation prompt.)
pub const CLAUDE_CONTINUATION_EXIT: i32 = 2;

/// Exit code every other outcome maps to, regardless of tool.
pub const HOOK_OK_EXIT: i32 = 0;

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }

    #[test]
    fn hook_input_deserializes_minimal_claude_payload() {
        let raw = r#"{
            "session_id": "742f6a04-f174-409a-ab01-419a16c5f372",
            "cwd": "/repo",
            "stop_hook_active": false
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed.session_id,
            sid("742f6a04-f174-409a-ab01-419a16c5f372")
        );
        assert_eq!(parsed.cwd, "/repo");
        assert!(!parsed.stop_hook_active);
        assert!(parsed.transcript_path.is_none());
        assert!(parsed.last_assistant_message.is_none());
    }

    #[test]
    fn hook_input_deserializes_full_claude_payload_ignoring_extras() {
        // Real-world claude hook stdin we captured during the
        // E1 experiment. Includes extras (`effort`, `permission_mode`,
        // `background_tasks`, `session_crons`) that core ignores.
        let raw = r#"{
            "session_id": "bcb7104f-7b4c-4715-8c3d-f1c020760d06",
            "transcript_path": "/Users/llfourn/.claude/projects/-x/y.jsonl",
            "cwd": "/private/tmp/clank-hook-experiment/claude",
            "permission_mode": "auto",
            "effort": {"level": "xhigh"},
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": "Hello! How can I help you today?",
            "background_tasks": [],
            "session_crons": []
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed.session_id,
            sid("bcb7104f-7b4c-4715-8c3d-f1c020760d06")
        );
        assert_eq!(
            parsed.transcript_path.as_deref(),
            Some("/Users/llfourn/.claude/projects/-x/y.jsonl")
        );
        assert_eq!(
            parsed.last_assistant_message.as_deref(),
            Some("Hello! How can I help you today?")
        );
        // Empty `background_tasks` == genuinely idle turn.
        assert!(parsed.background_tasks.is_empty());
        assert!(!parsed.has_non_wait_background_work());
        assert_eq!(parsed.background_wait_liveness(None), Liveness::Dead);
    }

    #[test]
    fn live_background_task_means_paused_not_idle() {
        // Captured claude Stop stdin while a `run_in_background` shell
        // was still running. A non-empty `background_tasks` is claude's
        // "paused waiting for background work to wake me back up" signal:
        // the stop hook must yield, not wait.
        let raw = r#"{
            "session_id": "bcb7104f-7b4c-4715-8c3d-f1c020760d06",
            "cwd": "/repo",
            "stop_hook_active": false,
            "background_tasks": [
                {"id": "bz32mj8uz", "type": "shell", "status": "running",
                 "description": "Sleep for 8 seconds in background", "command": "sleep 8"}
            ],
            "session_crons": []
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.background_tasks.len(), 1);
        assert_eq!(parsed.background_tasks[0].id.as_deref(), Some("bz32mj8uz"));
        assert_eq!(
            parsed.background_tasks[0].status.as_deref(),
            Some("running")
        );
        // A non-wait process: live work, but no `clank wait` among it.
        assert!(parsed.has_non_wait_background_work());
        assert_eq!(parsed.background_wait_liveness(None), Liveness::Dead);
    }

    #[test]
    fn missing_background_tasks_field_is_idle() {
        // Codex omits the field entirely; absence must read as idle, not
        // paused (so codex sessions always reach the wait).
        let raw = r#"{
            "session_id": "019e5385-ed97-7603-8561-dd9024328ff9",
            "cwd": "/repo",
            "stop_hook_active": false
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert!(parsed.background_tasks.is_empty());
        assert!(!parsed.has_non_wait_background_work());
        assert_eq!(parsed.background_wait_liveness(None), Liveness::Dead);
    }

    fn task(command: &str) -> BackgroundTask {
        BackgroundTask {
            id: None,
            status: Some("running".into()),
            command: Some(command.into()),
        }
    }

    fn input_with(tasks: Vec<BackgroundTask>, stop_hook_active: bool) -> HookInput {
        HookInput {
            session_id: sid("bcb7104f-7b4c-4715-8c3d-f1c020760d06"),
            cwd: "/repo".into(),
            transcript_path: None,
            stop_hook_active,
            last_assistant_message: None,
            background_tasks: tasks,
        }
    }

    #[test]
    fn clank_run_desc_recognises_the_invocation_not_the_words() {
        assert_eq!(
            clank_run_desc("clank run --desc \"test run\" -- cargo test").as_deref(),
            Some("test run")
        );
        assert_eq!(
            clank_run_desc("/Users/x/.cargo/bin/clank run --desc 'test run' -- make").as_deref(),
            Some("test run")
        );
        assert_eq!(
            clank_run_desc("cd /repo && exec clank run --desc=build -- ./x").as_deref(),
            Some("build")
        );

        // Substring matching is NOT identity: each of these names the
        // same words and is not a run.
        assert_eq!(clank_run_desc("echo \"test run\""), None);
        assert_eq!(clank_run_desc("cargo test # test run"), None);
        assert_eq!(clank_run_desc("clank wait --author claude"), None);
        assert_eq!(
            clank_run_desc("clank attending x --desc \"test run\""),
            None
        );

        // Everything after `--` belongs to the CHILD. Its `--desc` is
        // not clank's and must not be picked up.
        assert_eq!(
            clank_run_desc("clank run --desc \"ours\" -- cargo test --desc \"theirs\"").as_deref(),
            Some("ours")
        );
        assert_eq!(
            clank_run_desc("clank run -- cargo test --desc \"theirs\""),
            None,
            "a run with no --desc of its own matches nothing"
        );
    }

    #[test]
    fn live_run_matches_counts_only_live_runs_with_that_exact_description() {
        let input = |tasks: serde_json::Value| -> HookInput {
            serde_json::from_value(serde_json::json!({
                "session_id": "sess-run-match",
                "cwd": "/tmp",
                "stop_hook_active": false,
                "background_tasks": tasks,
            }))
            .unwrap()
        };
        let run = |cmd: &str| serde_json::json!({"id": "t", "command": cmd});

        let one = input(serde_json::json!([run(
            "clank run --desc \"test run\" -- cargo test"
        )]));
        assert_eq!(one.live_clank_runs("test run").len(), 1);
        assert_eq!(
            one.live_clank_runs("test run")[0].id.as_deref(),
            Some("t"),
            "the unique match carries the harness id the marker cannot"
        );
        assert_eq!(one.live_clank_runs("other").len(), 0);

        // A finished task is not evidence that anything is running.
        let done = input(serde_json::json!([{
            "id": "t",
            "status": "completed",
            "command": "clank run --desc \"test run\" -- cargo test"
        }]));
        assert_eq!(done.live_clank_runs("test run").len(), 0);

        // Two live runs sharing a description cannot be told apart, so
        // the caller's "exactly one" rule declines to suppress rather
        // than picking either.
        let two = input(serde_json::json!([
            run("clank run --desc \"test run\" -- cargo test"),
            run("clank run --desc \"test run\" -- cargo build"),
        ]));
        assert_eq!(two.live_clank_runs("test run").len(), 2);
    }

    #[test]
    fn is_clank_wait_matches_real_command_shapes() {
        for cmd in [
            "clank wait",
            "clank wait --repo /x --author claude",
            "/Users/x/.cargo/bin/clank wait",
            "./clank wait",
            "cd /repo; clank wait",
            "cd '/repo with spaces' && clank wait --author claude",
            "false || /Users/x/.cargo/bin/clank wait",
            "cd /repo\nclank wait",
            "bash -lc 'cd /repo; clank wait'",
            "sh -lc 'cd /repo && clank wait'",
            "zsh -lc 'cd /repo; clank wait --author claude'",
            "CLANK_DIR=/repo clank wait",
            "env CLANK_DIR=/repo clank wait",
            "exec clank wait",
            "command clank wait",
            "(clank wait)",
        ] {
            assert!(task(cmd).is_clank_wait(), "should match: {cmd}");
        }
        for cmd in [
            "clank status",
            "clank waitx",
            "clankwait",
            "sleep 12",
            "git wait",
            "clank",
            "echo clank wait",
            "echo 'clank wait'",
            "grep \"clank wait\" file",
            "printf 'cd /repo; clank wait'",
            "# clank wait",
            "echo done # clank wait",
            "command -v clank wait",
            "env CLANK_DIR=/repo echo clank wait",
            "bash -lc 'echo clank wait'",
            "zsh -lc 'grep \"clank wait\" file'",
        ] {
            assert!(!task(cmd).is_clank_wait(), "should NOT match: {cmd}");
        }
        // No command (non-shell task) is never a clank wait.
        assert!(
            !BackgroundTask {
                id: Some("x".into()),
                status: Some("running".into()),
                command: None,
            }
            .is_clank_wait()
        );
    }

    #[test]
    fn background_disposition_covers_the_state_machine() {
        use Tool::{Claude, Codex};
        // A live process, no clank wait, claude → cli must peek for work
        // (regardless of stop_hook_active — no dependence on it now).
        assert_eq!(
            background_disposition(Claude, &input_with(vec![task("sleep 60")], false), None),
            BgDisposition::NeedsWorkCheck
        );
        assert_eq!(
            background_disposition(Claude, &input_with(vec![task("sleep 60")], true), None),
            BgDisposition::NeedsWorkCheck
        );
        // Codex has no run_in_background auto-wake → yield, never peek/nudge.
        // A sleep on codex yields, but nothing here is a waker: no
        // wait, no registration. The variant says so, and the turn-end
        // classifier is not free to read it as one
        // (the-turn-end-is-one-decision).
        assert_eq!(
            background_disposition(Codex, &input_with(vec![task("sleep 60")], false), None),
            BgDisposition::LiveWorkNoWake
        );
        // No background work → proceed to the normal wait.
        assert_eq!(
            background_disposition(Claude, &input_with(vec![], false), None),
            BgDisposition::NoBackgroundWork
        );
    }

    /// The registration is recognised STRUCTURALLY, not found as a
    /// substring.
    ///
    /// Every case here contains the token and none of them runs the
    /// registered wait, so a `contains` accepted all of them and the
    /// hook yielded to an unrelated task — reintroducing exactly the
    /// false proof the token exists to close (codex 6402a09).
    #[test]
    fn a_registration_is_the_invocation_not_a_substring() {
        // THE REAL THING, in the shapes a launch actually takes.
        for cmd in [
            "clank wait --tag abc123",
            "clank wait --tag=abc123",
            "cd /repo && clank wait --tag abc123",
            "/usr/local/bin/clank wait --author rev --tag abc123",
            "env FOO=1 clank wait --tag abc123",
        ] {
            assert_eq!(
                registered_wait_tag(cmd).as_deref(),
                Some("abc123"),
                "should register: {cmd}"
            );
        }

        // Registers a DIFFERENT token: real invocations, wrong tag.
        // A prefix is not the token, and neither is one in another
        // segment.
        for (cmd, tag) in [
            ("clank wait --tag abc123ff", "abc123ff"),
            ("echo abc123; clank wait --tag different", "different"),
        ] {
            assert_eq!(registered_wait_tag(cmd).as_deref(), Some(tag), "{cmd}");
            assert_ne!(registered_wait_tag(cmd).as_deref(), Some("abc123"), "{cmd}");
        }

        // Registers NOTHING: no reachable `clank wait --tag` at all.
        for cmd in [
            // The token in a comment runs nothing.
            "clank wait # --tag abc123",
            // …nor in another command's arguments.
            "echo abc123 && sleep 1",
            // Past `--` the flag belongs to something else.
            "clank wait -- --tag abc123",
            // DETACHED inside the task's own shell: the listed task
            // completes while the waiter lives on, so its completion
            // says nothing about the wait.
            "clank wait --tag abc123 &",
            "clank wait --tag abc123 >out 2>&1 &",
            // Not a wait at all.
            "clank run --desc x -- clank wait --tag abc123",
        ] {
            assert_eq!(
                registered_wait_tag(cmd),
                None,
                "must register NOTHING: {cmd}"
            );
        }

        // An EMPTY operand registers nothing rather than an empty
        // token — which every command string would then contain.
        // Asserting only "not abc123" here passes for `Some("")`,
        // which is exactly how this hid from its own mutant.
        for cmd in [
            "clank wait --tag ''",
            "clank wait --tag=",
            "clank wait --tag \"\"",
        ] {
            assert_eq!(
                registered_wait_tag(cmd),
                None,
                "empty is not a token: {cmd}"
            );
        }

        // DETACHMENT WITHOUT A `&`, and a `#` that is not a comment.
        // Both came from using TWO parsers that disagreed: one read
        // `issue#123` as starting a comment and so never saw the
        // trailing `&`, while the other read the assignment correctly
        // and found the wait; and one turned `(` into a generic
        // boundary, so an asynchronous process substitution looked
        // like a foreground command (codex 8fac98f).
        for cmd in [
            "CLANK_NOTE=issue#123 clank wait --tag abc123 >out 2>err &",
            "NOTE=a#b clank wait --tag abc123 &",
            "true <(clank wait --tag abc123 >out 2>err)",
            "cat >(clank wait --tag abc123)",
            "(clank wait --tag abc123)",
            "X=$(clank wait --tag abc123)",
            "X=`clank wait --tag abc123`",
            "clank wait --tag abc123 | tee out",
            // A HERE-DOCUMENT's body is text, not commands — and a
            // denylist of operators read it as one (codex 836b939).
            "cat <<'END'\nclank wait --tag abc123\nEND",
            // `coproc { … }` launches asynchronously; the block, the
            // keyword and the braces are each outside the form.
            "coproc {\nexec clank wait --tag abc123\n}",
            "{ clank wait --tag abc123 ; }",
            "if true; then clank wait --tag abc123; fi",
            "for i in 1; do clank wait --tag abc123; done",
            // MULTILINE bodies, where the wait gets a segment of its
            // own and its program really is `clank`. Skipping the
            // keyword segments and searching on is what accepted
            // these; one-line forms miss the case entirely, which is
            // how a surviving mutant read as unreachable code rather
            // than missing coverage (codex 98579b8).
            "coproc if true; then\nexec clank wait --tag abc123\nfi",
            "if false; then\nclank wait --tag abc123\nfi",
            "for i in; do\nclank wait --tag abc123\ndone",
            "while true; do\nclank wait --tag abc123\ndone",
            // Two waits in one launch: which one is the listed task?
            "clank wait --tag abc123; clank wait --tag abc123",
            // A `clank wait` with no registration at all in a launch
            // that also has one is not a form to reason about.
            "clank wait; clank wait --tag abc123",
            // Redirections are NOT modelled. They do not detach, but
            // clank never asks for one, and a form this grammar
            // cannot read literally is refused rather than guessed.
            "clank wait --tag abc123 >out 2>err",
            "clank wait --tag abc123 >out",
            // An expansion's value is unknowable from here.
            "cd $HOME && clank wait --tag abc123",
        ] {
            assert_eq!(
                registered_wait_tag(cmd),
                None,
                "unsupported or detaching form must register NOTHING: {cmd}"
            );
        }

        // And the forms that DO keep the wait in the foreground —
        // including a `#` inside a value with no `&` after it, which
        // is what distinguishes the comment rule from a blanket one.
        for cmd in [
            "CLANK_NOTE=issue#123 clank wait --tag abc123",
            "cd /repo || true; clank wait --tag abc123",
            "cd /repo && clank wait --tag abc123",
            "env -- clank wait --author rev --tag abc123",
        ] {
            assert_eq!(
                registered_wait_tag(cmd).as_deref(),
                Some("abc123"),
                "should register: {cmd}"
            );
        }

        // `&&` sequences; it detaches nothing.
        assert_eq!(
            registered_wait_tag("clank wait --tag abc123 && true").as_deref(),
            Some("abc123"),
        );
        // A QUOTED metacharacter is data, but by the time a word is
        // tokenized its quotes are gone and `a&b` is indistinguishable
        // from an operator. The grammar refuses rather than guesses —
        // over-narrow, and the cost is a nudge for a form clank never
        // asks for.
        assert_eq!(
            registered_wait_tag("clank wait --author 'a&b' --tag abc123"),
            None,
        );
    }

    /// A listed `clank wait` takes the turn only when the
    /// REGISTRATION names it.
    ///
    /// A `BackgroundTask` carries `id`, `status` and `command` — no
    /// pid, no process identity — so nothing ties a listed task to a
    /// running process on its own, and three attempts to infer that
    /// tie each proved something else (codex 07c7d8d). The token is
    /// in both places by construction: the command the agent ran, and
    /// the claim the wait published.
    #[test]
    fn a_listed_wait_takes_the_turn_only_when_the_registration_names_it() {
        use Tool::Claude;
        let tagged = |cmd: &str| input_with(vec![task(cmd)], false);

        // UNREGISTERED: the agent's wait is listed and running, and
        // that is still not evidence.
        let plain = tagged("clank wait");
        assert_eq!(plain.background_wait_liveness(None), Liveness::Unknown);
        // Positively, and it is not the branch you would guess: an
        // unregistered wait is invisible here (a listed wait is not
        // "non-wait background work"), so the turn falls through to
        // the normal deliver-work/park path. "not a yield" was
        // satisfied by every other variant too, and said none of this.
        assert_eq!(
            background_disposition(Claude, &plain, None),
            BgDisposition::NoBackgroundWork,
            "an unregistered wait must leave the agent a continuation",
        );
        // A claim exists but the listed command does not carry it:
        // still a guess, so still Unknown.
        assert_eq!(
            plain.background_wait_liveness(Some("abc123")),
            Liveness::Unknown,
        );

        // REGISTERED: one live listed task carries the token.
        let armed = tagged("clank wait --tag abc123");
        assert_eq!(
            armed.background_wait_liveness(Some("abc123")),
            Liveness::Live
        );
        assert_eq!(
            background_disposition(Claude, &armed, Some("abc123")),
            BgDisposition::YieldToWait {
                tag: "abc123".into()
            },
            "the turn yields to a wait it can name, and carries the name",
        );

        // COMPLETED is the one thing the harness does know, and it
        // outranks the token.
        let mut done = tagged("clank wait --tag abc123");
        done.background_tasks[0].status = Some("completed".into());
        assert_eq!(
            done.background_wait_liveness(Some("abc123")),
            Liveness::Dead
        );

        // AMBIGUOUS: two live tasks carrying one token makes "which
        // process is this?" a guess again.
        let two = input_with(
            vec![
                task("clank wait --tag abc123"),
                task("clank wait --tag abc123"),
            ],
            false,
        );
        assert_eq!(
            two.background_wait_liveness(Some("abc123")),
            Liveness::Unknown
        );

        // THE JOIN ITSELF is structured, not a substring search. Each
        // of these contains the token and runs no registered wait, so
        // a `contains` would yield the turn to an unrelated task —
        // and testing the recogniser alone cannot see that, because
        // the recogniser is not what the hook calls.
        for cmd in [
            "clank wait --tag abc123ff",
            "clank wait # --tag abc123",
            "echo abc123; clank wait --tag different",
            "clank wait --tag abc123 &",
            // The forms whose launching shell exits while the waiter
            // lives on (codex 8fac98f, 836b939).
            "CLANK_NOTE=issue#123 clank wait --tag abc123 >out 2>err &",
            "true <(clank wait --tag abc123 >out 2>err)",
            "cat <<'END'\nclank wait --tag abc123\nEND",
            "coproc {\nexec clank wait --tag abc123\n}",
            "coproc if true; then\nexec clank wait --tag abc123\nfi",
            "if false; then\nclank wait --tag abc123\nfi",
            "for i in; do\nclank wait --tag abc123\ndone",
        ] {
            assert_eq!(
                input_with(vec![task(cmd)], false).background_wait_liveness(Some("abc123")),
                Liveness::Unknown,
                "the join must not accept: {cmd}",
            );
        }
        // And an empty registration names nothing, however many tasks
        // are listed.
        assert_eq!(
            input_with(vec![task("clank wait --tag abc123")], false)
                .background_wait_liveness(Some("")),
            Liveness::Unknown,
        );
    }

    #[test]
    fn hook_input_deserializes_full_codex_payload_ignoring_extras() {
        // Captured codex stdin. Extras: `turn_id`, `model`,
        // `permission_mode`, `hook_event_name`.
        let raw = r#"{
            "session_id": "019e5385-ed97-7603-8561-dd9024328ff9",
            "turn_id": "019e5386-1684-7250-a7fc-1823f4b8c1be",
            "transcript_path": "/Users/llfourn/.codex/sessions/rollout.jsonl",
            "cwd": "/private/tmp/clank-hook-experiment/codex",
            "hook_event_name": "Stop",
            "model": "gpt-5.5",
            "permission_mode": "default",
            "stop_hook_active": false,
            "last_assistant_message": "Hello. How can I help?"
        }"#;
        let parsed: HookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed.session_id,
            sid("019e5385-ed97-7603-8561-dd9024328ff9")
        );
        assert!(!parsed.stop_hook_active);
    }

    #[test]
    fn hook_input_rejects_missing_required_field() {
        let raw = r#"{"cwd":"/x","stop_hook_active":false}"#;
        let result: Result<HookInput, _> = serde_json::from_str(raw);
        assert!(result.is_err(), "expected missing session_id to fail");
    }

    #[test]
    fn hook_input_rejects_invalid_session_id() {
        let raw = r#"{
            "session_id": "not/a/valid/id",
            "cwd": "/x",
            "stop_hook_active": false
        }"#;
        let result: Result<HookInput, _> = serde_json::from_str(raw);
        assert!(result.is_err(), "expected invalid session id to fail");
    }

    #[test]
    fn codex_block_decision_serializes_correctly() {
        let decision = CodexBlockDecision::from_reason("run wait now");
        let json = serde_json::to_string(&decision).unwrap();
        assert_eq!(json, r#"{"decision":"block","reason":"run wait now"}"#);
    }

    #[test]
    fn codex_block_decision_escapes_reason_with_specials() {
        let decision = CodexBlockDecision::from_reason("line 1\n\"quoted\"\tend");
        let json = serde_json::to_string(&decision).unwrap();
        // serde_json handles all the JSON escaping for us.
        assert_eq!(
            json,
            r#"{"decision":"block","reason":"line 1\n\"quoted\"\tend"}"#
        );
    }

    #[test]
    fn hook_outcome_variants_are_distinct() {
        // Sanity that we can match exhaustively on the public enum.
        let outcomes = [
            HookOutcome::Continue { reason: "x".into() },
            HookOutcome::RenewWait,
            HookOutcome::Silent {
                why: SilentReason::AutoOff,
            },
            HookOutcome::Diagnostic {
                message: "y".into(),
            },
        ];
        for o in &outcomes {
            match o {
                HookOutcome::Continue { .. } => {}
                HookOutcome::RenewWait => {}
                HookOutcome::Silent { .. } => {}
                HookOutcome::Diagnostic { .. } => {}
            }
        }
    }

    #[test]
    fn continuation_exit_codes_are_correct() {
        assert_eq!(CLAUDE_CONTINUATION_EXIT, 2);
        assert_eq!(HOOK_OK_EXIT, 0);
    }
}
