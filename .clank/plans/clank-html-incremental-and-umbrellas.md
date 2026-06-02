# clank-html-incremental-and-umbrellas

Two improvements to `clank html`:

1. **Incremental builds** — don't re-fold the whole repo and
   re-render every page on each `clank html`. Pick up from
   where the last build left off, render only the new
   commits + a small re-check window for the top of the
   prior timeline.
2. **Plan-umbrella timeline + relative timestamps** —
   stop repeating the plan name on every row; group adjacent
   same-plan commits into a single visual umbrella. Add
   optional JS that swaps raw timestamps for "4h ago"
   strings, with the raw ISO load-bearing when JS is off.

## Incremental builds

### What gets persisted

The existing `index.html` is the cache.

Add a marker in the document head:

```html
<meta name="clank:last-built-sha" content="<full-head-sha>">
<meta name="clank:builder-version" content="0.0.1">
```

On the next `clank html`:

- Read the existing `.clank/html/index.html`.
- If it doesn't exist, or `clank:builder-version` mismatches,
  do a full rebuild (the current path). The version meta is
  the escape hatch when the renderer's HTML shape changes.
- Otherwise read the previous head SHA. Run
  `rebuild_from(repo, Some(prev_head), &new_head)` to fold
  ONLY the commits between the two heads. That's the
  incremental slice.

### Rendering the slice

- Per-commit pages: write one for each new event in the
  slice. Don't touch the prior commit pages.
- Index timeline: parse the existing `<div class="timeline">`
  block out of the document (see "HTML container shape"
  below for why it's a `<div>`, not `<ol>`). Splice in the
  new rows per the boundary rules below. Write the
  document back. Update the `clank:last-built-sha` meta.
- Subjects for the slice come from a fresh
  `collect_subjects(repo, Some(new_head))` scoped to the new
  range only (cheap: one `git log` call against the slice).

### Umbrella boundary merge

The timeline is rendered NEWEST-FIRST: the top of the DOM
is the most recent event, the bottom is the oldest. So the
boundary between "stuff already on the page" and "new
slice rows being spliced in" sits between:

- the BOTTOM of the new slice's grouping (its OLDEST
  umbrella — i.e., the one that, in render order, will sit
  just above the prior top), AND
- the prior index's TOPMOST umbrella.

If those two umbrellas share a plan key, they're the same
cycle and must merge. Blindly prepending the full new
grouping would otherwise leave two adjacent `[foo]`
umbrellas where there should be one.

Rule the splicer applies, in order:

1. Read the prior index's TOPMOST umbrella: its
   `data-umbrella-key` attribute (`foo`, `bar`, `ad-hoc`,
   etc.).
2. Group the NEW slice's events into umbrellas using the
   same grouping rule as a full rebuild, ordered
   chronologically (oldest first). Call the OLDEST new
   umbrella `slice_tail` and the NEWEST `slice_head`.
3. If `slice_tail.key == prior_top.key`, MERGE: take the
   rows in `slice_tail` and INSERT them at the TOP of the
   prior topmost umbrella's row list (still newest-first
   inside the umbrella — the new rows are more recent
   than the existing ones).  Then prepend every OTHER new
   umbrella (everything except `slice_tail`) above the
   prior topmost umbrella, ordered newest-first.
4. Otherwise: no merge. Prepend every new umbrella above
   the prior topmost umbrella, ordered newest-first.

Concrete example codex flagged: prior top is `[foo] revise`
inside a `[foo]` umbrella. New slice in chronological
order: `[foo] impl` then `[bar] intro` (bar is newer).
Grouping: `slice_tail = [foo] impl`, `slice_head = [bar]
intro`. Merge fires: `[foo] impl` goes to the top of the
existing `[foo]` umbrella's row list (above `[foo]
revise`). Then `[bar]` is prepended above the merged
`[foo]` umbrella. Final render: `[bar]` umbrella, then
`[foo]` umbrella containing `[foo] impl` then
`[foo] revise`. Exactly one `[foo]` umbrella; rows newest-
first within it.

Tag every umbrella with `data-umbrella-key=` at render
time so the splicer doesn't have to re-derive group
identity from text.

### HTML container shape

`<section class="umbrella">` inside an `<ol class="timeline">`
is invalid HTML — `<ol>` only accepts `<li>` children. Two
clean options:

- (a) `<div class="timeline">` containing
  `<section class="umbrella">` blocks containing
  `<div class="row">` rows. Loses the implicit ordered-list
  semantic but the visual order is clear without it.
- (b) Keep `<ol class="timeline">` with `<li class="umbrella">`
  wrappers, each carrying an internal `<header>` + a
  nested `<ol class="rows">` of `<li class="row">`. More
  semantically correct but heavier markup.

Decision: ship (a). Simpler, easier to splice, and the
"timeline" semantic is already implicit from the heading.
Document this on the renderer so a future maintainer
doesn't reach for `<ol>` again by reflex.

### Feedback re-check window

Feedback can land on old commits after the prior build —
codex might approve commit X half an hour after the html
was generated. Re-rendering them tile-by-tile is cheap if
we bound the work:

- Re-scan feedback for the TOP N commits of the prior
  timeline (N = 10). For each, if the feedback set has
  changed, rewrite BOTH:
  1. The `<section class="reviews">` block on that
     commit's per-commit page, AND
  2. The `<span class="marks">` element on that commit's
     row in the index `<div class="timeline">`.
- Plus the full feedback scan for every new slice commit
  (their pages and rows are being written for the first
  time, so no diffing needed).

For (2), tag each row with `data-sha=<full-sha>` at render
time. The splicer finds the row by selector and replaces
just the marks span — surgical, no row reflow.

Feedback for commits older than the top 10 of the prior
timeline is not re-checked. Trade-off explicitly: stale
feedback on deep history rarely matters; if it does, the
user can run `clank html --rebuild` (a flag, see below) to
force a clean pass.

### Progress bar

Stderr only, doesn't pollute stdout (which says `wrote <path>`).

Three phases worth a counter:

1. Folding the slice (count: commits in slice).
2. Writing per-commit pages (count: events in slice).
3. Patching index + top-N feedback re-check (count: N).

Style: `clank html: [###     ] 12/40 writing commit pages` —
fixed-width bar, in-place update via `\r`. Disable
auto when stderr isn't a TTY (use `is_terminal`). Add a
`--quiet` flag (or just `CLANK_PROGRESS=off`) for CI runs.

### New flags

- `clank html --rebuild` — ignore the cache marker, force a
  full pass. Same effect as deleting `.clank/html/` first.
- `clank html --quiet` — suppress the progress bar.

### Edge cases

- Prior head no longer reachable (force-push, branch reset
  destroyed it): `rebuild_from` will fail or produce
  garbage. Detect this (`git merge-base --is-ancestor
  <prev> <new>`); if not an ancestor, fall back to full
  rebuild.
- Re-finalized plans across the slice boundary: the slice
  events include the finalize, the per-commit pages get
  written normally. No special handling.
- Adoption gate just shipped — the cache version bump on
  RepoState (CACHE_FORMAT_VERSION 7) is already taken care
  of by the existing state-cache path; html's own cache is
  separate.

## Plan-umbrella timeline + relative timestamps

### Visual model

Today every row carries the plan badge, so the columns
shift around the variable-length plan name. Move the plan
label OUT of the rows and into a parent group:

```
┌── [foo] ─────────────────────────────┐
│  abc1234  intro   "foo intro"   ✓✓   │
│  def5678  plan    "revise"      ✓    │
│  9abcdef  code    "impl"        ✓    │
└──────────────────────────────────────┘
┌── ad-hoc ────────────────────────────┐
│  1111111  ad-hoc  "drive-by"    ●    │
└──────────────────────────────────────┘
┌── [bar] ─────────────────────────────┐
│  2222222  intro   "bar intro"        │
└──────────────────────────────────────┘
```

Each umbrella wraps a contiguous run of same-plan events
(or `ad-hoc` for the unattributed code commits). A new
umbrella starts when the plan attribution changes. A single
plan can spawn multiple umbrellas if interleaved with
other plans or ad-hoc commits.

### Subject prefix stripping (core, not html)

The umbrella header already says `[foo]`, so the
`[foo] subject` prefix on each row is pure noise. But this
parsing belongs in `clank-core` — there's already a
`parse_title_prefix(subject: &str) -> Option<TitlePrefix>`
at `crates/core/src/repo_state.rs:317` that the classifier
uses, and the html layer shouldn't re-parse / re-strip the
same syntax.

Refactor (or add a sibling helper) in core:

```rust
pub struct ParsedSubject<'a> {
    pub prefix: Option<TitlePrefix>,
    /// Subject with the recognized `[…] ` prefix removed.
    /// Equals the raw input when no prefix matched.
    pub body: &'a str,
}

pub fn parse_subject(subject: &str) -> ParsedSubject<'_>;
```

Existing classifier path: rewrite the one site that calls
`parse_title_prefix` to read `parse_subject(...).prefix`.
Renderers (`clank log`, `clank html`) call `parse_subject`
directly and render `.body`. No re-stripping, no string
juggling, no re-finding the `]`.

Observable behavior in `clank html`:

- Subject `[foo] intro` under any plan umbrella renders as
  `intro`. The plan badge is already in the umbrella
  header.
- Subject `[foo,bar] shared work` renders as `shared work`
  in either the `foo` or `bar` umbrella.
- Subject without a recognized prefix renders verbatim.
- Per-commit page header reads the same parsed body, so
  `<h2 class="subject">` doesn't repeat `[<plan>]` either.

### CSS shape

- `.umbrella` is a `<section>` with a left border + soft
  background tint indexed by plan name (hash → CSS custom
  property) so multiple cycles of the same plan share a
  visual identity without needing to repeat the badge.
- The plan badge becomes the umbrella's `<header>` —
  rendered once at the top.
- Rows inside lose their `.plan-pill` column; the grid
  collapses to `5rem 4.5rem auto 1fr auto auto` minus the
  plan column, so SHAs / kinds / subjects / marks /
  timestamps line up cleanly.
- `.umbrella + .umbrella` has reduced top spacing so the
  flow reads as a list of cycles, not a list of cards.

### Relative timestamps with graceful degradation

Each `.ts` element gets a `data-iso` attribute carrying the
canonical timestamp; the element's text is the same ISO
string by default:

```html
<time class="ts" data-iso="2026-06-02T07:20:34Z">2026-06-02T07:20:34Z</time>
```

A tiny inline `<script>` at the bottom of `index.html`
walks `[data-iso]` and rewrites text to `4h ago` /
`3d ago` / `2 months ago` using a small formatter (no
dependency; ~30 lines of vanilla JS). If JS is disabled,
the raw ISO is visible — load-bearing fallback per the
user's framing.

The script is inline (no `style.css`-equivalent) so the
build stays one CSS file + two HTML pages. The umbrella
pages and the index share it.

## Surfaces touched

- `crates/core/src/repo_state.rs`:
  - Add `ParsedSubject<'a>` struct and `parse_subject(&str)
    -> ParsedSubject<'_>` returning prefix + body.
  - Re-implement `parse_title_prefix` as a thin wrapper
    over `parse_subject(...).prefix` (or delete it and
    migrate the classifier call site directly — pick at
    implementation time, whichever leaves fewer callers
    on the old name).
  - Unit tests: bracketed-single, bracketed-multi,
    `[misc]`, no prefix, empty brackets, whitespace
    before / inside brackets — same shapes
    `parse_title_prefix` tests today, extended to assert
    `.body`.
- `crates/cli/src/cli/html.rs`:
  - Read existing `index.html` and parse the meta markers
    when present.
  - Refactor `render_index` into three independent chunks
    that can be re-rendered in isolation:
    1. `render_status_header(&StatusSnapshot) -> String`
    2. `render_timeline_umbrellas(&[LogEvent], ...) -> String`
       (full or partial; the splicer feeds it slice events)
    3. `render_row(event, reviews, subject) -> String`
       (used for individual marks/row rewrites)
  - Implement the slice path:
    a. ALWAYS re-render the status header and replace the
       existing `<header class="status">` block, even on
       empty-slice incremental runs. Feedback gates,
       last-finished state, queue count, blocks, dirty
       bit, and branch metadata can all shift without a
       new commit.
    b. Fold the slice via `rebuild_from(repo, Some(prev), &head)`.
    c. Write a per-commit page for each new event.
    d. Splice the slice's umbrellas into the existing
       `<div class="timeline">` per the boundary-merge
       rule above. Container is a `<div>`, NOT an
       `<ol>` (per "HTML container shape").
    e. For each of the top N prior-timeline commits whose
       feedback has changed, rewrite BOTH the per-commit
       page's `<section class="reviews">` AND that
       commit's `<span class="marks">` in the index
       (selected by `data-sha=`).
  - Group events into umbrellas during timeline rendering;
    each umbrella carries `data-umbrella-key=`.
  - Inline `<script>` for relative timestamps.
  - Add `--rebuild` and `--quiet` flags via `HtmlArgs`
    additions in `cli/mod.rs`.
- `crates/cli/src/cli/mod.rs::HtmlArgs` — `rebuild: bool`,
  `quiet: bool`.
- `crates/cli/src/cli/html.rs::CSS` — umbrella styles.
- Existing `html_integration` tests stay green; add new
  ones (see below).

## Tests

Incremental:

- `html_writes_meta_marker_with_head_sha` — fresh build
  inserts `<meta name="clank:last-built-sha" ...>` with
  the head sha.
- `html_incremental_build_only_writes_new_pages` — run
  `clank html`, add a new plan + commit, run `clank html`
  again; assert NEW commit pages exist and prior pages'
  mtimes are unchanged.
- `html_incremental_splices_new_rows_into_timeline` —
  parse the index after an incremental build; assert
  the new event row is above the prior top row.
- `html_incremental_rechecks_top_n_feedback` — seed
  feedback on a prior-build commit AFTER the first build;
  run the second build; assert the per-commit page's
  review section reflects the new feedback.
- `html_incremental_updates_index_verdict_marks` — same
  setup as above, but the assertion checks the INDEX
  timeline row's `<span class="marks">` for that commit
  also shows the new verdict (regression for the case
  where commit pages were updated but the index marks
  went stale).
- `html_incremental_refreshes_status_header_on_empty_slice`
  — codex's regression: run `clank html` to produce the
  initial site, then WITHOUT landing any new commit,
  write a fresh feedback file that flips a plan's gate
  (e.g. APPROVE → FINISHED, or first FINISHED on an
  unreviewed plan). Run `clank html` again. Assert the
  rendered `<header class="status">` reflects the new
  gate state (and last-finished / queue / blocks if
  affected). Pins "header always re-renders, slice or
  no slice."
- `html_incremental_merges_continuation_umbrella` —
  fold ends with `[foo] revise` as the topmost row inside
  a `[foo]` umbrella; add a new `[foo] impl` commit; run
  incremental; assert the rendered index has exactly ONE
  `[foo]` umbrella, containing the new `[foo] impl` row
  ABOVE the prior `[foo] revise` row (newest-first inside
  the umbrella).
- `html_incremental_starts_new_umbrella_on_plan_change` —
  prior tail is `[foo] revise`; new slice is
  `[bar] intro`; assert the rendered index has TWO
  umbrellas, `[bar]` above `[foo]`, in that order.
- `html_incremental_merges_then_starts_new_above` —
  codex's regression: prior tail is `[foo] revise`;
  new slice is `[foo] impl` then `[bar] intro` (bar is
  newest). Assert the rendered index has TWO umbrellas:
  `[bar]` on top, then a SINGLE `[foo]` umbrella below
  containing `[foo] impl` (top) and `[foo] revise`
  (bottom). Pins both the boundary-merge target
  selection and the inner-row prepend order.
- `html_timeline_uses_div_container_not_ol` — assert
  the rendered `class="timeline"` element is a `<div>`,
  not an `<ol>` (and that umbrella `<section>` blocks
  appear as its direct children; no `<li>` parents). This
  pins the valid-HTML decision so a future change can't
  silently regress it.
- `html_incremental_falls_back_to_full_rebuild_when_prev_head_not_ancestor`
  — write a marker with a SHA from a discarded branch;
  assert the build runs a full rebuild and overwrites
  the index cleanly.
- `html_rebuild_flag_forces_full_pass` — pass `--rebuild`;
  assert prior commit pages get re-written even when
  unchanged.

Umbrella:

- `html_timeline_groups_same_plan_into_umbrella` —
  fold a chain `[foo] intro` + `[foo] revise` + `[foo]
  impl`; assert the index has one `<section class="umbrella">`
  containing all three rows and ONE plan badge.
- `html_timeline_breaks_umbrella_on_plan_change` —
  `[foo] intro` + `[bar] intro` → two umbrellas.
- `html_timeline_ad_hoc_umbrella_label` — adopted repo
  with two consecutive ad-hocs; assert one umbrella with
  label "ad-hoc" containing both rows.
- `html_timeline_renders_parsed_subject_body` — seed
  commit `[foo] intro`; assert the index row's subject
  text is `intro` and the per-commit page header reads
  `intro`. (Behavior is observed at the html layer; the
  source of truth is `parse_subject` in core.)
- `html_timeline_renders_parsed_body_for_multi_plan` —
  commit `[foo,bar] shared work`; under the `foo`
  umbrella the subject reads `shared work`.
- `html_timeline_renders_raw_when_no_prefix` — ad-hoc
  commit titled `random fix`; assert the rendered subject
  is `random fix` verbatim (no over-aggressive stripping).
- Core unit tests on `parse_subject` cover the edge
  cases (whitespace, `[misc]`, empty brackets, mixed
  case) so the html tests don't need to retrace them.

Relative timestamps:

- `html_timestamps_carry_data_iso_attribute` — assert
  each `.ts` element has a `data-iso` attr equal to the
  rendered text (the JS fallback case).
- `html_emits_inline_relative_time_script` — assert the
  index contains an inline `<script>` block computing
  relative times (existence check is enough; not testing
  JS behavior).

## Out of scope

- A `clank html serve` watcher / live reload. Still v2;
  this plan keeps the static-files-only contract.
- Persisting reviewer feedback in the cache marker.
  Re-scanning the top N is cheap; storing hashes would
  add bookkeeping for marginal speedup.
- Pagination / infinite scroll for huge timelines. v2.
- Per-plan landing pages. The umbrella covers the "where
  do these commits cluster?" need without introducing a
  whole new page set.
- Replacing the tiny inline JS with a CSS-only relative
  time. Not feasible without precomputed bucketing at
  build time, which would stale the moment you load the
  page later.
