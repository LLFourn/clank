# status-doc-overlays

## Problem / current model

In `clank status --tui`, pressing **Enter** on a timeline entry opens the
full-window commit-detail overlay (`status-commit-detail`). The overlay
slot is hardcoded to commits: `detail: Option<CommitOverlay>`, and the
entry→overlay map (`scroll::entry_commit_sha`) only yields a commit sha.

Two entry kinds aren't served:

1. **A plan-name header** (`OnelineRow::Header { plan: Some(stem) }`) does
   nothing on Enter. It should open the plan's markdown document in the
   window, with the markdown rendered (not shown as raw `#`/`*` source).
2. **A review row** opens the commit overlay at the TOP (commit subject +
   message); you then have to scroll past the diff message to reach the
   feedback. Enter on a review should land you ON that reviewer's feedback.

## The model: one generic document overlay

The overlay is "a scrollable document filling the window." The entry under
the cursor decides which document. Make that the actual type rather than
special-casing commits:

- `scroll::entry_commit_sha` → **`scroll::entry_overlay_target`** returning
  `Option<OverlayTarget>`:
  ```
  enum OverlayTarget {
      Commit { sha: CommitSha, focus: Option<String> }, // focus = reviewer to land on
      Plan   { stem: String },
  }
  ```
  - `Commit` seg → `Commit { sha, focus: None }` (own sha, open at top).
  - `Review` seg → `Commit { sha, focus: Some(author) }` (same FORWARD
    positional scan to its commit, stopping at a Header/end as today; the
    review's author rides along as the scroll focus).
  - `Header { plan: Some(stem) }` → `Plan { stem }`. Ad-hoc header
    (`plan: None`), Ask, InProg → `None` (unchanged: nothing to open).
- The loop's single slot becomes `detail: Option<Overlay>` where
  ```
  struct Overlay { data: OverlayData, offset: usize }
  enum OverlayData { Commit(CommitDetail), Plan { stem: String, markdown: Option<String> } }
  ```
  `Overlay` keeps the existing `new/refresh(swaps data, preserves offset)/
  scroll(clamped)` discipline — one place, both kinds. This REPLACES the
  separate `CommitOverlay` (no second parallel Option, no second top-of-loop
  branch — that duplication is the smell this avoids).
- Top-of-loop overlay branch dispatches on `data`: render + `Refresh`
  re-fetch by identity (sha → `fetch_commit_detail`; stem →
  `read_plan_markdown`), preserving `offset`. The scroll/back key routing is
  identical for both, so `commit_detail_nav`/`CommitNav` are renamed
  `doc_nav`/`DocNav` (they were never commit-specific — only scroll + back).

## Part 2 — review → land on the feedback (single source of layout)

`render_commit_detail` is the only place that knows the detail layout (rule,
sha+subject, wrapped body, then per-reviewer blocks). To scroll to a
reviewer WITHOUT a second function re-deriving line positions (which would
drift), split out the layout:

- `build_commit_lines(data, cols) -> CommitLayout { lines: Vec<String>,
  review_line: BTreeMap<String, usize> }` — builds the content once and
  records the start line of each reviewer's block.
- `render_commit_detail` becomes: window `build_commit_lines(..).lines` by
  offset (unchanged output; covered by existing tests).
- Opening from a `Review`: `offset = layout.review_line.get(author)` (the
  reviewer's block top), falling back to the first reviewer / 0 if absent.
  Computed at the current `cols`; a later resize re-clamps like any offset.

So Enter on `codex`'s review opens the same overlay already scrolled to
`✗ codex` + its body.

## Part 1 — plan header → rendered markdown

- `read_plan_markdown(repo, stem) -> Option<String>`: read
  `.clank/plans/<stem>.md`, else `.clank/finished/<stem>.md`, else `None`
  (a plain working-tree file read like `fetch_commit_detail` already does
  for feedback files — not a git read, so the git-boundary layer is not
  involved). The stem is exactly the umbrella plan key, which is the file
  name.
- `render_plan_doc(stem, markdown: Option<&str>, offset, rows, cols)`:
  `region_rule(stem, "↑↓ scroll · Esc back", cols)`, blank, then the
  rendered markdown lines (or a dim "plan file not found" when `None`);
  windowed by offset like the commit view; returns `(lines, total)`.

### Markdown → terminal renderer (new pure module `markdown.rs`)

`render_markdown(md, cols) -> Vec<String>` over **pulldown-cmark** (already a
dependency for `clank html`). Supported constructs (the bar: a clank plan
reads well):

- **Headings** — bold, blank line after; level shown by a dim `#`×level
  prefix so depth survives in monochrome.
- **Inline**: `**bold**` → bold, `*em*`/`_em_` → italic, `` `code` `` →
  fixed-color span. (Adds a `Style::Bold` + `bold()` to the span model —
  the only inline style currently missing; emits `\x1b[1m`.)
- **Lists** — `•` bullets / `1.` ordered, nested by indent.
- **Block quote** — dim, `│ ` gutter.
- **Code block** (fenced/indented) — dim, left gutter, NOT word-wrapped
  (kept verbatim, hard-cut at cols).
- **Thematic break** — a dim rule line.
- **Links** — visible text kept; bare autolinks shown as their URL. (OSC 8
  hyperlinking is out of scope — keep it text.)

Wrapping must respect inline styling, so paragraphs wrap at the **styled-character**
level: fold inline events into a flat `Vec<(char, Style)>`, greedy-wrap to
`cols` breaking at the last space (hard-break an over-long token), then
coalesce equal-style runs per line into `Span`s and `emit`. This handles
abutting styles ("a`b`c"), wide chars, and word breaks with one correct
pass; reusing the plain `wrap` would lose the inline styles.

Out of scope for the renderer: tables, images, footnotes, HTML blocks,
syntax highlighting (a code block is uniformly dim). These degrade to their
plain text, never raw markup noise.

## Acceptance

- Enter on a plan header opens the plan's markdown rendered in the window
  (headings bold, lists bulleted, inline code/bold/italic styled), scrollable,
  Esc back; a header whose file is missing shows "plan file not found".
- Enter on a review opens the commit overlay already scrolled to that
  reviewer's feedback block; Enter on the commit row opens at the top
  (unchanged).
- One overlay slot (`Option<Overlay>`), one render/refresh/scroll/nav path
  for both kinds; `Refresh` re-fetches by identity and preserves the scroll
  offset for plan docs too (the watcher fires constantly).
- Tests (pure where possible): `entry_overlay_target` over a representative
  seq (commit→own sha/None focus; review→commit sha + author focus; review
  before a section break → None; header→Plan{stem}; ad-hoc header/ask/inprog
  → None); `build_commit_lines` records each reviewer's start line and the
  windowing is unchanged; `read_plan_markdown` plans-then-finished
  precedence + missing → None; `render_markdown` styles a doc exercising
  heading/bold/italic/code/list/quote/code-block/rule and wraps a long
  styled paragraph within cols. `cargo test -p clank --lib` green; fmt +
  clippy clean.

## Out of scope

- Editing plans from the TUI (read-only, like every other overlay).
- A markdown scrollbar / search / link activation.
- Rendering historical plan content at a commit (always the current
  working-tree file).
