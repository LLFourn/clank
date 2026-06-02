# clank-html

Render the clank event log + current status to a static HTML
site at `.clank/html/`, with a per-commit drilldown showing the
diff and reviewer feedback (or, for plan-only commits, the
rendered plan revision). Goal: skim the timeline in a browser,
drill down to any commit's review and diff in two clicks.

## UX

Two-page model:

1. **`index.html`** — status header + reverse-chronological
   timeline. Each timeline row is a clickable link to its
   commit page. Status header shows what `clank status`
   shows: branch, HEAD short SHA, dirty flag, active plans
   list with gate state + waiting-on, last-finished plan,
   queue count, blocks. Compact, scannable; small reviewer
   verdict marks (✓ APPROVE / ✓✓ FINISHED / ✗ CHANGES) next
   to each commit row.

2. **`commit/<sha>.html`** — per-commit drilldown:
   - Header: full SHA, parents, author, date, attribution
     (`[<plan>] subject`), gate state, finalize status.
   - Reviewer feedback: one card per review (color-coded by
     verdict), rendered markdown body, author label.
   - Diff: unified diff, file-by-file collapsible, syntax
     coloring on the additions/deletions backgrounds (no
     full syntax highlighting in v1 — keep it static).
   - For plan-only commits (intro / revise of plan text): the
     centerpiece is the rendered plan markdown AT THIS
     COMMIT — not the diff. Diff still rendered below in a
     collapsed section.

## Command surface

- `clank html` — build the site at `.clank/html/`. Idempotent;
  re-running overwrites existing pages.
- `clank html --open` — build, then `open`/`xdg-open` on the
  generated `index.html`.
- `clank html open` — same as `--open` but spelled as a
  subcommand for discoverability. Decide one canonical
  spelling and add the other as a clap alias.

`clank html` accepts `--repo PATH` like other commands. Exit
code 0 on success.

Open design question (deferred to v2): `clank html serve`
would start a local HTTP server + filesystem watcher + auto-
rebuild on change. NOT in v1 — static files only. The plan
should leave the output structure friendly to a future watcher
(stable URLs, deterministic page set).

## Output layout

```
.clank/html/
  index.html
  style.css                  -- shared, tiny
  commit/
    <full-sha>.html          -- one per commit in the log
  assets/                    -- empty for now; reserve the path
```

- `.clank/html/` is gitignored. Add `/html/` to
  `CLANK_GITIGNORE_BODY` in `crates/cli/src/init_facts.rs` and
  to `CLANK_GITIGNORE_LEGACY_BODIES` so re-running `clank
  init` upgrades old repos.
- Stable filenames (`commit/<full-sha>.html`) so bookmarks
  survive rebuilds.
- No JS framework. Optional tiny inline `<script>` for
  expand/collapse on diff file sections; works without JS
  (graceful fallback: everything expanded).

## Data sources

- Status: `crate::cli::status::StatusSnapshot::build_async`
  (already exists). Render its fields directly; no parallel
  fold logic.
- Timeline: `RepoState::log_events` (same source `clank log`
  uses). Iterate in reverse chronological order.
- Per-commit diff: shell out to `git show --pretty=format: -p
  <sha>` and parse into file sections. Use existing
  `crate::git_io` helpers where they fit.
- Per-commit feedback: `FsReviewLookup::reviews_for(sha)`
  returns the verdict list; read the body of each
  `.clank/agents/<author>/feedback/<ref>.md` for the rendered
  text.
- Plan-at-commit body: `git show <sha>:.clank/plans/<stem>.md`
  for plan-only events; render via pulldown-cmark.

## Rendering

- **Markdown → HTML**: `pulldown-cmark` (well-maintained,
  pure-Rust). Wrap output in a `<article class="md">` with
  CSS targeting typical markdown elements (paragraph
  spacing, headings, code blocks, lists, blockquotes).
- **Diff → HTML**: parse the git unified diff into
  per-file sections (path, mode/rename header, hunks).
  Render each hunk as `<table class="diff">` with three
  columns: old line number, new line number, content. Color
  background on `+`/`-` lines. Collapse files with a
  details/summary element (works without JS).
- **HTML escaping**: every piece of data that isn't already
  trusted HTML goes through a small `escape_html` helper.
  Reviewer markdown bodies are escaped at the source side,
  THEN markdown-rendered (pulldown-cmark handles this
  correctly via its own escaping).

## Visual design

Frontend-design principles applied to a Rust-generated static
site (no framework, no design system to fight with):

- **Type**: system font stack with `font-feature-settings:
  "tnum"` for tabular numerics in SHAs and dates. Comfortable
  line-height (~1.5) for prose, tighter (~1.25) for timeline
  rows.
- **Color**: neutral, not chrome-y. Use color sparingly to
  encode meaning — green for APPROVE, indigo for FINISHED,
  red for REQUEST_CHANGES, dim for unreviewed. Diff lines get
  pale-green / pale-red backgrounds at low saturation so a
  long diff is still readable.
- **Layout**: single max-width column (~880px) for
  readability, with status header pinned at the top. Timeline
  rows are dense but not cramped — 1.6em row height. Plan
  badges (`[<stem>]`) get a subtle pill background to
  distinguish from commit subjects.
- **Affordances**: timeline rows are entire-row links (the
  `<a>` wraps the row). Hover state changes background, not
  underline-only. Verdict marks have tooltips with author
  names.
- **Empty states**: no plans? show "No active plans — repo
  is idle." No feedback on a commit? "No reviewer has
  weighed in." Not chatty banners.
- **Print-friendly**: a quick `@media print` block hides
  hover states + collapses expand toggles. Useful for
  archiving.

The CSS lives in `style.css` (one shared file referenced from
every page) and is small enough to inline if we want a single
self-contained `index.html` later.

## Surfaces touched

- `crates/cli/src/cli/html.rs` (new) — command handler,
  page builders, asset writer.
- `crates/cli/src/cli/mod.rs` — `HtmlArgs`, `HtmlCmd::Open`
  variant (or `--open` flag), `Commands::Html` wire-up.
- `crates/cli/src/main.rs` — dispatch.
- `crates/cli/Cargo.toml` — add `pulldown-cmark` (latest
  4.x), probably `html-escape` too (or hand-rolled escape).
- `crates/cli/src/init_facts.rs` —
  `CLANK_GITIGNORE_BODY` gains `/html/\n`; previous body
  moves to `CLANK_GITIGNORE_LEGACY_BODIES` so existing
  repos silently upgrade.

## Tests

- `html_build_writes_index_and_one_commit_page` — seed a
  repo with a plan intro commit, run `clank html`, assert
  `.clank/html/index.html` exists and references
  `commit/<sha>.html` which exists.
- `html_index_shows_status_header_and_timeline_rows` —
  parse the index HTML, assert status fields appear and
  every log event has a corresponding `<a>` link.
- `html_commit_page_for_plan_only_renders_markdown` —
  plan-only intro; assert the commit page contains the
  rendered plan body (e.g. `<h1>plan-stem</h1>`).
- `html_commit_page_for_code_commit_renders_diff` — code
  commit; assert the page contains a diff table and the
  expected file path.
- `html_commit_page_renders_feedback_with_verdict_marks`
  — seed feedback with APPROVE and FINISHED on the same
  commit; assert both cards appear with the right
  verdict label.
- `html_escapes_user_content` — feedback body contains
  `<script>alert(1)</script>`; assert the rendered HTML
  has it escaped (no raw `<script>` tag in the output).
- `html_open_flag_invokes_opener` — mock the opener (or
  use a feature flag to swap it for a recording stub);
  assert `clank html --open` builds AND calls the opener
  with the index path. (Plain integration test that uses
  a process-launching opener is too platform-specific;
  use an injected hook instead.)
- `init_gitignore_includes_html_dir` — after `clank init
  --yes`, the `.clank/.gitignore` body includes `/html/`.

## Out of scope

- **Serve mode (`clank html serve`).** v2. Decide whether
  to ship a built-in static-file server + watcher or just
  document a `python -m http.server` workaround. For now
  static files + browser file:// URL are enough.
- **Syntax highlighting on diffs.** Plain monospace +
  +/- backgrounds in v1. Syntect / tree-sitter later if
  someone asks.
- **Search / filtering on the timeline.** Cmd-F in the
  browser is fine for v1. A small client-side filter is a
  later enhancement.
- **Side-by-side diffs.** Unified is enough; side-by-side
  is a v2 polish.
- **Rendering finished plans.** They live in
  `.clank/finished/<stem>.md`; the per-commit page for the
  finalize commit can link to them, but no dedicated
  per-plan landing page in v1.
- **Cross-repo dashboard at `~/.clank/html/`.** The user
  mentioned `~/.clank/html` initially; if we ever want
  a cross-repo index that aggregates per-repo sites, that's
  a separate feature. v1 is repo-local only.
- **Auto-rebuild on stop-hook / git events.** No machinery
  hooks into the build; `clank html` is purely on-demand.
