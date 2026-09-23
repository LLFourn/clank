# a-tool-call-says-what-it-did

The web page names the work, not the JSON the tool was called with.

> "In the remote web ui it renders:
> `{"command":"for f in .clank/agents/*/feedback/8267076*.md; do echo
> \"=== $f\"; cat \"$f\"; done","description":"Read intro review
> feedback"}`
> but that's not right. It should unpack this json and render the
> description as the current activity (possibly with the ability to
> expand to see the actual command)." — lloyd

## Why

A tool call arrives as a name and an input, and the input is an
object. `transcript.rs` keeps it as `i.to_string()` — the whole
object, compact, one line — and the page draws the first 120
characters of that into the collapsed row
(`page.html`: `summary .in` ← `b.input.slice(0, 120)`), with the
same string whole in the expansion. So the one line that says what
an agent is doing right now spends itself on `{"command":"for f in
.clank/agents/*/fe…` and stops before the verb.

The call already carries the sentence: Claude's Bash calls take a
`description`, and every other tool has one field that is the point
of the call — the path read, the pattern searched, the URL fetched.
Which field that is, is knowledge about tools, and knowledge about
tools belongs in Rust where it can be tested, not in a slice index
in a `<script>`.

## The build

**A call is a title and a detail.** Parsing a `tool_use` derives
both beside the raw input it keeps: `title`, the single line the
collapsed row shows, and `detail`, the call as something to read —
a Bash command as its own text with its newlines, not a JSON string
literal with `\n` in it. The raw input stays on the turn, so
nothing that reads a call loses what was actually sent.

**The title is per-tool knowledge with one fallback.** `description`
for the tools that take one (Bash, Task); the path for `Read`,
`Edit`, `Write`, `NotebookEdit`; the pattern for `Grep` and `Glob`;
the URL for `WebFetch`; the command for codex's `exec`/`shell`,
which send a plain string already. A tool nobody taught it — an MCP
server's, a new one — takes its first short string field, and an
input with nothing string-shaped takes the tool's own name. Raw JSON
is never a title.

**The page shows the title.** The collapsed row draws `title`, the
expansion draws `detail`, and every other place the UI says what an
agent is doing draws the title too. A turn from before this existed
has neither field and falls back to the input it always had, the way
the prose turn falls back when it has no `html`.

## Tests

- A Bash call with a description titles as the description; without
  one, as its command; and its detail is the command's own text,
  newlines and all, with no JSON punctuation.
- `Read`, `Edit`, `Grep`, `WebFetch` title as the path, the path, the
  pattern and the URL.
- codex's `exec` string input is untouched: title and detail are the
  command.
- An unknown tool with an object input titles as a field of it, and an
  input with no string field titles as the tool's name — in neither
  case a brace.
- A long title is cut to a line's worth; the detail is not.
- The page draws `title` in the collapsed row and falls back to
  `input` for a turn that has none.

## Out of scope

- Tool OUTPUT rendering. This is about the call.
- The TUI's log rows, which name plans and commits, not tool calls.
