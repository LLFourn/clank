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
- Index timeline: parse out the existing `<ol class="timeline">`
  content, prepend the new event rows above the existing
  rows (newest-first stays correct), and write the
  document back. Update the `clank:last-built-sha` meta.
- Subjects for the slice come from a fresh
  `collect_subjects(repo, Some(new_head))` scoped to the new
  range only (cheap: one `git log` call against the slice).

### Feedback re-check window

Feedback can land on old commits after the prior build —
codex might approve commit X half an hour after the html
was generated. Re-render those tile-by-tile is cheap if we
bound the work:

- Re-scan feedback for the TOP N commits of the prior
  timeline (N = 10). For each, if the feedback set has
  changed (compare against the verdict marks rendered in
  the prior index — easiest is to look up the per-commit
  page's review section and re-write it from the new
  scan).
- Plus the full feedback scan for every new slice commit.

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

- `crates/cli/src/cli/html.rs`:
  - Read existing `index.html` and parse the meta markers
    when present.
  - Implement the slice path: fold from prev_head, render
    new commit pages, splice new rows into the timeline,
    re-render the top-N feedback sections in their per-
    commit pages.
  - Refactor `render_index` into separate header + timeline
    chunks so splicing only touches the timeline `<ol>`.
  - Group events into umbrellas during timeline rendering.
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
