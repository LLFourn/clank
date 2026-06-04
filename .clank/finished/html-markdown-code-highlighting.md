# html-markdown-code-highlighting
# Apply syntect highlighting to fenced code blocks in `clank html`-rendered markdown

## Problem

`clank html` syntax-highlights diff source lines (each row of a commit-detail diff goes through `crate::cli::html_highlight::highlight_line` at `html.rs:1118`), but markdown-rendered content (plan bodies + feedback bodies at `html.rs:768`, `:800`, `:939`) does NOT. Fenced code blocks in plans and feedback render as plain `<pre><code>` with HTML-escaped content but no syntect classes.

The asymmetry is jarring: a plan that contains a Rust snippet to motivate a change ships with a colorful diff view but a monochrome plan view of the same code.

The plumbing to fix this already exists:

- syntect is in the dep tree (`Cargo.toml` cites the no-onig pure-Rust backend).
- `highlight_line` wraps `ClassedHTMLGenerator` with the `hl-` class prefix.
- The CSS classes live in the embedded stylesheet at `html.rs:1622` (search for "syntect class colors"). The diff path and the new markdown path will share the same theme.

## Verified before promotion

- **Render path**: `render_markdown` at `html.rs:1177` uses `pulldown_cmark::html::push_html(&mut out, parser)` — the shortcut path that emits HTML directly from the event stream without giving us a hook for `CodeBlock` events. To intercept fenced blocks we have to either (a) walk the events manually and emit HTML ourselves, or (b) preprocess by transforming `Event::Start(Tag::CodeBlock(_))` … `Event::End` ranges in the event stream before `push_html`.

- **`highlight_line` shape**: takes `lang_hint: Option<&str>` (a file extension like "rs") and a single `line: &str`. Internally it loops `LinesWithEndings::from(...)`, so multi-line input doesn't crash, but the contract is per-line — the trailing-newline-strip at the end was designed for the diff row container, not multi-line blocks. A new `highlight_block(lang_hint, source)` function that owns block-shaped output is cleaner than overloading `highlight_line`.

- **Markdown fence info strings vs syntect lookups**: pulldown-cmark gives us the info string verbatim (e.g. ` ```rust`, ` ```rs`, ` ```bash`, ` ``` `). syntect's `SyntaxSet` exposes `find_syntax_by_extension(ext)` AND `find_syntax_by_name(name)` AND `find_syntax_by_token(tok)`. We need both lookups so `rust` and `rs` both work. Plan-and-feedback-typical languages worth verifying: `rust`/`rs`, `bash`/`sh`, `python`/`py`, `js`/`ts`, `toml`, `json`, `md`/`markdown`, `diff`. syntect's default grammar set covers all of these.

- **No info string**: untyped ` ``` ` fences are common. Fall back to `find_syntax_plain_text` — same plain-but-safe escape behavior the diff path uses.

- **Event-stream filter behavior**: the existing parser already filters `Event::Html` and `Event::InlineHtml` out (`html.rs:1190`). That filter has to keep working when we change the rendering loop. Code-block content arrives as `Event::Text` between `Tag::CodeBlock` start/end and is NOT one of the filtered HTML variants, so we don't have to think about that interaction.

## Approach

1. **Add `highlight_block(lang_hint: Option<&str>, source: &str) -> String`** to `html_highlight.rs`. Same `ClassedHTMLGenerator` + `hl-` prefix as `highlight_line`, but:
   - Accepts the entire block as one `&str`.
   - Loops `LinesWithEndings` over the whole source.
   - Returns the inner HTML fragment (no `<pre>`/`<code>` wrapper — caller decides).
   - Same fail-safe: if syntect chokes or the lang is unknown, fall back to a plain-escaped string.
   - Resolve lang via `find_syntax_by_token(name)` first (catches `rust`/`bash`/`python`), then `find_syntax_by_extension(name)` (catches `rs`/`sh`/`py`), then plain-text fallback.

2. **Rewrite `render_markdown`** at `html.rs:1177` to consume events manually instead of relying on `push_html`. The shape:
   - Walk events.
   - For non-code events: forward to `push_html` via a temporary parser, OR (simpler) build a transformed event iterator and pass the whole thing to `push_html`.
   - For `Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))` … `Event::End(TagEnd::CodeBlock)`: buffer the `Event::Text` body, call `highlight_block`, emit `<pre><code class="hl">…</code></pre>` directly. Skip the surrounding `Start`/`End` events when reconstructing the stream for `push_html`.

   The cleanest implementation is probably to walk the iterator manually with a small state machine, emitting HTML directly to the output buffer and using `push_html` for non-code sub-ranges. That keeps the existing HTML-filter behavior (the filter still applies to non-code events).

3. **No CSS additions needed**. The `.hl-…` classes already cover the syntect output. Verify by rendering a plan with a `rust` block and visually inspecting that tokens get colors.

4. **Preserve `class="hl"` on the wrapper** so the existing diff `.hl` styles (background, padding, font) apply to code blocks too. Or use a different wrapper class (`.hl-block`?) if the diff styles include row-specific padding that looks wrong for full blocks. Implementer's call after looking at the existing CSS.

## Out of scope

- Adding new languages to syntect's grammar set. The `default-fancy` feature covers what plan authors actually use.
- Line numbering inside code blocks. Plans don't number; not needed.
- Inline `code` highlighting (single-backtick spans). Inline code stays unhighlighted — the diff path doesn't highlight inline tokens either, and tokenizing 5-char spans is rarely informative.
- Custom themes. Stylesheet already defines the colors; this plan inherits them.

## Acceptance

- A fenced code block with a recognized language in a plan body OR a feedback body renders with `hl-` classes around its tokens. Inspecting the HTML output for ` ```rust\nfn main() {}\n``` ` shows at least one `<span class="hl-…">…</span>` element.
- A fenced code block with no info string (untyped ` ``` `) renders safely with escaped text and no syntect crash.
- An unrecognized info string (e.g. ` ```weird-lang `) falls back to plain text (same fail-safe `highlight_line` already has).
- The HTML-filter behavior is preserved: inline `<script>` tags in non-code markdown still get stripped.
- `cargo test --workspace` passes. The existing `highlights_rust_keyword`, `unknown_extension_falls_back_to_plain_text`, and `empty_line_does_not_panic` tests stay green and continue covering `highlight_line`'s per-line contract; new tests cover `highlight_block`.
- Visual sanity-check: open a real plan with a fenced rust block in the rendered HTML and confirm coloring.

## Tests

In `crates/cli/src/cli/html_highlight.rs` (unit):

- `highlight_block_rust_multiline_has_hl_classes`: pass a 3-line Rust block, assert output contains `hl-` AND all three lines' content.
- `highlight_block_unknown_lang_falls_back`: same fall-back guarantee as `highlight_line`.
- `highlight_block_no_lang_safe_escape`: `<` and `&` in plain text get escaped, no syntect crash.
- `highlight_block_resolves_rust_via_name_AND_via_extension`: both `"rust"` and `"rs"` map to the same Rust grammar.

In `crates/cli/src/cli/html.rs` (the existing test module):

- `render_markdown_highlights_fenced_block`: pass `"\n```rust\nfn main() {}\n```\n"` to `render_markdown`, assert the output contains `class="hl-`.
- `render_markdown_untyped_fence_is_plain_safe`: ` ```\n<div>raw</div>\n``` ` renders as escaped text inside `<pre><code>` (no `hl-` classes, no raw HTML passthrough).
- `render_markdown_html_filter_still_runs`: an inline `<script>alert(1)</script>` in regular markdown (not inside a fence) still gets stripped (regression-guard the existing filter).

## Implementation note

The event-walking rewrite of `render_markdown` is the only structural change. Once that's in place, the syntect call site is a single line. Most of the diff will be the test additions.

If the event-walker turns out fiddly (pulldown-cmark's `Tag`/`TagEnd` are mildly verbose), a pragmatic alternative is to preprocess the markdown source: regex-match fenced blocks, substitute placeholder tokens, render through `push_html`, then string-replace the placeholders with highlighted blocks. Less elegant but smaller diff. Implementer's call.
