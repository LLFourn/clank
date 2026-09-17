# the-transcript-reads-like-a-document
# The transcript reads like a document

## Why

> "here's a side by side of what claude code looks like vs clank's UI.
> can you make our one styled like claude code's text… Also change to a
> light theme."
>
> "also notice the centering and the text width (text width is larger
> and nicer on claude app)."

Side by side, the difference is not decoration. On the left, prose is
prose: proportional type, headings, lists, inline code set apart, a
centered column at a comfortable measure. On the right, ours is the
JSONL with a font applied — every character the same width, markdown
syntax showing as literal asterisks and backticks, the column pinned
left at `max-width: 86ch`.

We already render markdown properly. `clank html` has done it all
along.

## The model

**The transcript is prose the agent wrote, not a log we tail.**

The page treats an agent's text as a `pre-wrap` monospace blob because
that is what the terminal shows, and the terminal is where it came
from. But the text is markdown — written to be rendered — and the
crate already has a renderer with exactly the right properties:

`html.rs::render_markdown` (used to build the `clank html` site)
routes fenced blocks to `html_highlight::highlight_block` and drops
`Event::Html` and `Event::InlineHtml`. That is NOT the same as being
safe to hand to `innerHTML`, and codex was right to stop the plan on
it. Measured against the installed renderer:

```
[open](javascript:alert%281%29)   → <a href="javascript:alert%281%29">open</a>
[open](JaVaScRiPt:alert(1))       → <a href="JaVaScRiPt:alert(1)">open</a>
[dat](data:text/html;base64,…)    → <a href="data:text/html;base64,…">dat</a>
a <div>x</div> and <script>alert(1)</script>
                                  → <p>a x and alert(1)</p>
```

Two defects, one of them ours to have assumed away:

1. **Destinations are escaped, not validated.** pulldown-cmark's
   `escape_href` makes a destination safe as an ATTRIBUTE; it says
   nothing about the scheme. Ordinary markdown — no raw HTML anywhere —
   produces an executable link in the authenticated remote origin.
2. **Dropping a raw tag keeps its text.** `<script>alert(1)</script>`
   becomes the words `alert(1)` in the prose. On the plan site that is
   a silent corruption of what someone wrote; in a TRANSCRIPT, where
   agents discuss HTML constantly, it makes the page lie about what was
   said.

So the renderer gains a stated contract, and both surfaces get it:

- **A destination allow-list**, case-insensitive: `http`, `https`,
  `mailto`, and destinations with no scheme at all (relative paths,
  fragments). Everything else keeps its words and loses its tag —
  unlinked text, not a blanked `href=""`, which would still be a link
  to this page. Written as an allow-list it needs no separate defence
  against obfuscation: `java\tscript:` is not `javascript`, and it is
  not `https` either.
- **Raw HTML is ESCAPED, not dropped.** `<div>` renders as the
  characters `<div>`. That is what a transcript of someone discussing
  HTML has to do, and it is what the plan site should have done all
  along.

So this is mostly a matter of asking the function we already have.
The rule that keeps it honest:

- **Prose is rendered. Everything quoted verbatim stays verbatim.**
  An agent's and a person's message text become markdown. Tool input,
  tool output and the terminal view stay preformatted text set with
  `textContent`, because their value is that they are exact.

## Deliverables

1. **Text turns carry rendered HTML, made server-side.**
   `render_markdown` becomes `pub(crate)` and the transcript's `text`
   bodies gain a rendered form beside the raw text. Rendering happens
   in Rust, not in the page: the renderer exists, it already refuses
   raw HTML, and the page stays free of a markdown library.

2. **Proportional type for prose, monospace where monospace means
   something.** Body text in the system UI stack; `code`, fenced
   blocks, tool input and output, and the terminal in the mono stack.
   Martian Mono stays the agent's name.

3. **A centered column at a readable measure.** The turns column
   centers in the viewport rather than hugging the left edge, with a
   measure of roughly 65–75 characters of RENDERED prose — which is
   wider in words than today's `86ch` of monospace, and the reason the
   reference reads better.

4. **A light palette.** The existing tokens (`--base`, `--bezel`,
   `--edge`, `--ink`, `--muted`) take light values; nothing that reads
   them changes.

5. **The terminal view stays dark**, on its own ground, inside the
   light page. Agent TUIs pick ANSI colours assuming a dark terminal —
   bright white on white, grey dimming to invisible. A light page
   cannot fix that, and inverting it would break every agent's output
   at once.

6. **The person's turn keeps its own block**, restyled for the light
   ground: who is speaking must stay readable without a label.

7. **Consecutive tool calls collapse into one row.**

   > "also see how it does a nice 'run 2 commands' rather than showing
   > all the bash inline."

   A run of tool turns with no prose between them is one disclosure
   row — `Ran 3 commands` — that opens to the individual calls, each
   with the input and output it has now. A run of one keeps the row it
   has today; a run still in flight says so, because the last tool
   having no output is the only live "working" signal the page has.
   The grouping is derived in the page from the turn list: no server
   change, and no turn is hidden — reading a transcript should show
   what was said, with what was DONE one tap away.

## Tests

- **The destination allow-list**, which is the reason this plan needed
  a second look: `javascript:`, `JaVaScRiPt:`, `java<tab>script:`,
  `data:text/html`, `data:image/png`, `vbscript:` and `file://` all
  render as unlinked text, while `https://`, `HTTPS://`, `mailto:`,
  `./relative`, `?q=a:b` and `#fragment` stay links. The
  page sets this HTML with `innerHTML`, so the contract is the thing to
  prove; mutation-check by loosening the check and watching each case
  fail.
- **Raw HTML is escaped, not dropped**: a message containing
  `<script>alert(1)</script>` shows those characters, and the words
  `alert(1)` do not appear as prose.
- A fenced block's bytes survive exactly, including leading whitespace
  and markdown characters inside it.
- Tool output is still set as text, never as markup: a tool that prints
  `<b>` shows `<b>`.
- In the browser harness: the measure at 320/390/960/1400px, the column
  centered, and the light tokens in force. Each mutation-checked.
- Grouping, in the harness: three tool turns between two messages make
  ONE row that opens to three; prose between them makes two rows; a
  group whose last tool has no output says it is still running; and
  expanding a group leaves every input and output reachable.
- The existing page contracts (the ids, the facts fields) still pass.

## Out of scope

- A dark/light toggle. The ask is a light theme; the tokens make the
  other one cheap later if the phone at night wants it back.
- The composer at the bottom of the page — its own plan.
- WHICH turns are shown. Harness turns stay hidden, everything else
  stays present; only the grouping of tool calls changes.
