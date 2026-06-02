# clank-html-fancy-diffs

The current `clank html` diff renderer is brutal: plain
monospace, no syntax highlighting, no line numbers, and the
hunk lines have visible vertical gaps between them. Make
diffs the centerpiece they should be.

## Three concrete fixes

1. **Syntax highlighting at build time.** Use `syntect` to
   colorize diff lines according to the file's language
   (inferred from extension). Output is CSS-classed
   `<span>`s, themed light/dark via the existing
   `prefers-color-scheme` switch in `style.css`.
2. **Line numbers, two columns** (old / new). Unified diff
   already carries them in the hunk header (`@@ -A,B +C,D
   @@`); compute per-line numbers from there.
3. **Tighten the row layout.** The current visible gap is
   the combination of `font: .../1.4` line-height plus the
   `\n` separator that gets rendered as a space when the
   `<span>`s aren't wrapped tight. Render lines as
   `<div class="line">` (no internal `\n`s) inside a
   `<pre>` shell that owns line-height 1.25; let the row
   coloring fill the row entirely.

## What "fantastic diffs" looks like

```
┌── crates/cli/src/cli/html.rs ─────────────────────┐
│  42  42    fn render_file_patch(fp: &FilePatch) … │
│  43  43        let mut s = String::new();         │
│  -   44   -    s.push_str("<pre …>");             │
│  44   +    s.push_str("<table class=\"diff\">");  │
│  45  45        for hunk in &fp.hunks {            │
└───────────────────────────────────────────────────┘
```

Column layout (CSS grid inside each hunk):

- col 1 (old line #): right-aligned tabular, 3-4 ch wide
- col 2 (new line #): right-aligned tabular, 3-4 ch wide
- col 3 (sign): `+` / `-` / ` `, 1 ch
- col 4 (source): `<code>` with syntect-classed tokens

Background colors live on the row (`.line.add` /
`.line.del`) so the whole strip fills, line-number gutter
included. Hunk headers (`@@ -A,B +C,D @@`) get a quieter
band — secondary text, no gutter, full-width.

## Syntect integration

- Add `syntect = "5"` to `crates/cli/Cargo.toml`. Default
  features are heavy (~7MB of compiled grammars); use
  `default-features = false` + `features = ["default-fancy"]`
  — or, cheaper, manually pick the subset that covers the
  languages clank repos actually use (Rust, TOML, MD, JSON,
  YAML, shell). Decide at implementation time.
- Build a single `SyntaxSet` + `ThemeSet` at process start
  (cheap, ~50ms); reuse across files.
- Per file: pick the syntax by extension (`syntect`'s
  `find_syntax_by_extension`). Fall back to plain text when
  unknown.
- For each hunk line, strip the leading sign (`+`/`-`/` `),
  pass the rest through syntect's `ClassedHTMLGenerator`
  (CSS-class output mode, not inline styles). Re-attach
  the sign in its own column.

Theme choice: emit `class` form. Ship one CSS block in
`style.css` that defines colors for the syntect class set
under `prefers-color-scheme: light` and the dark equivalent
for dark mode. Two themes hand-tuned to match the existing
palette — don't drop in a syntect default theme verbatim,
they're too saturated next to the existing UI chrome.

## Line numbers

Parse `@@ -A,B +C,D @@` for the starting line numbers, then:

- Context line (` `): both columns advance.
- Add (`+`): only the new column advances; old column shows
  blank.
- Delete (`-`): only the old column advances; new column
  shows blank.

Use `right`-aligned numbers in `.line-num` with
`font-feature-settings: "tnum"` so digits line up.

## Tightening the row layout

Root cause of the visible gap today:

```css
.hunk { font: .8rem/1.4 var(--mono); ... }
.line { display: block; padding: 0 .25rem; }
```

`line-height: 1.4` plus the `\n` between spans renders as
text that has both leading AND a literal newline glyph.
With a `pre` wrapper the newlines become hard breaks but
each row still carries the 1.4 height.

Fix:

- Change `<span class="line">…</span>\n` → `<div
  class="line">…</div>` (no trailing `\n` in the HTML).
- Set `.hunk` to a grid container with `line-height: 1.25`.
- Drop `<pre>` — use `<code>` only for the source column.
  The grid handles row alignment.

## Surfaces touched

- `crates/cli/src/cli/html.rs::parse_unified_diff` — also
  capture the hunk header's starting line numbers so the
  renderer can compute per-line indices. Add `old_start`
  and `new_start` fields to `FilePatch` (or a new `Hunk`
  struct).
- `crates/cli/src/cli/html.rs::render_file_patch` — emit
  the new grid markup with per-line `old_num`, `new_num`,
  `sign`, `source`.
- New module `crates/cli/src/cli/html_highlight.rs` — owns
  the `SyntaxSet` / `ThemeSet` singletons + a small
  `highlight_line(language, text) -> String` helper.
- `crates/cli/src/cli/html.rs::CSS` — diff grid + syntect
  class color rules for light/dark.
- `crates/cli/Cargo.toml` — `syntect` dep.

## Tests

- `diff_renders_with_line_numbers` — seed a 3-line code
  change, assert the rendered HTML has the expected
  old/new line numbers for context/add/del rows.
- `diff_renders_with_syntect_classes` — code commit
  touching a `.rs` file; assert the source column
  contains syntect's CSS class names (e.g.
  `class="source rust"` and at least one
  `<span class="hl …">` substring).
- `diff_unknown_extension_falls_back_to_plain_text` —
  touch a `.weird` file; assert the diff still renders
  (no syntect class spans, but the row layout is intact).
- `diff_lines_have_no_vertical_gap` — pure visual smoke
  test: assert the rendered `.line` divs have NO trailing
  `\n` between them in the HTML (no `</div>\n<div`
  sequence in the patch body). The CSS change isn't
  directly testable, but the markup-side fix is.
- `diff_hunk_header_has_quieter_band` — assert the
  `@@ … @@` line is rendered with the `hunk-hdr` class
  and spans both gutters (no line numbers shown).

## Out of scope

- Word-level intra-line highlighting (Github's "green
  word inside a green line"). Useful, separate plan.
- Side-by-side diff view. Unified is enough; side-by-side
  is a v3 enhancement.
- Collapsing large unchanged regions. The `<details>`
  per-file collapse already covers the "skip this file"
  case; intra-hunk folding adds little for typical
  reviews.
- Linking line numbers to source on disk
  (`commit/<sha>.html#L42`). Cute, but the diff is the
  centerpiece, not a navigation hub.
- Replacing the existing `dim-gray header for diff =
  pale-bg for adds/dels` palette with anything richer.
  Two-tone diffs read well; saturated diff palettes fight
  the rest of the UI.
