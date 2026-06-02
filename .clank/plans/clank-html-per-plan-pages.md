# clank-html-per-plan-pages

Each plan should have its own landing page under
`.clank/html/plan/<stem>.html`:

- Timeline of THIS plan's events at the top (just the
  events tied to the plan, in newest-first order).
- The most recent revision of the plan body rendered
  underneath in beautiful markdown.

Every plan-pill in the timeline (umbrella header on
`index.html`, the `plan-line` on a commit page) becomes a
clickable link to that plan's page.

## UX

```
┌──────────────────────────────────────────────┐
│  [foo]                                       │
│  ↳ active · gate: finished · waiting on master │
│                                              │
│  Timeline                                    │
│  abc1234  intro   "intro"     2026-06-02 ✓✓  │
│  def5678  plan    "revise"    2026-06-02 ✓   │
│  9abcdef  code    "impl"      2026-06-02 ✓✓  │
│  fedcba9  finish  "finish"    2026-06-02     │
│                                              │
│  ──── Plan (latest revision) ────────────    │
│  # foo                                       │
│  …rendered markdown body…                    │
└──────────────────────────────────────────────┘
```

## Data sources

- Events: filter `events` (already in `build_site`) by
  `event_plan(event) == Some(stem)`.
- Plan body: the LATEST commit-time tree's
  `.clank/plans/<stem>.md` (or `.clank/finished/<stem>.md`
  if the plan is finalized). Use `git show
  <head_sha>:<path>` for active plans; for finished
  plans, the path is `.clank/finished/<stem>.md` at HEAD.
- Plan status: derived from
  `StatusSnapshot::plans`/`last_finished` — gate,
  `waiting_on`, finished marker.
- Subjects, reviews: existing maps in `build_site`.

## Routing

- `.clank/html/plan/<stem>.html` — one per active plan +
  one per finished plan.
- Index page's umbrella headers wrap their plan badge in
  `<a href="plan/<stem>.html">` for active plans, or
  `<a href="plan/<stem>.html">` for finished plans the
  user might still want to reference.
- Commit page's existing `<div class="plan-line">plan:
  <span class="plan-pill">…</span></div>` becomes
  `<a class="plan-pill" href="../plan/<stem>.html">`.

## Render shape

Reuse existing components:

- Header: similar to the index's `<header class="status">`
  but plan-scoped (one row of metadata: state, gate,
  waiting-on, last activity).
- Timeline: reuse `render_timeline(&events_for_plan,
  &reviews, &subjects)` — the umbrella grouping will
  produce a single umbrella containing this plan's events,
  which is fine (the plan badge is redundant in the header
  but the row format is identical).
  - Optional simplification: a per-plan variant
    `render_plan_timeline` that omits the umbrella wrapper
    altogether (the page header IS the plan label) and
    emits the rows flat. Decide at implementation time;
    visual outcome matters more than reuse.
- Plan body: `<section class="plan-body"><article class="md">
  {markdown}</article></section>` — same shape the per-
  commit page already uses.

## Incremental

Plan pages need to be regenerated when:

- Any event for the plan landed in the slice → rewrite the
  page. Compute the set of plans touched by slice events.
- Any top-N feedback re-check changed reviews on a
  plan-attributed commit → rewrite that plan's page too.
- The plan's body text changed (i.e., its last revision
  commit is in the slice) → already covered by case (1).

For simplicity, just compute `affected_plans = {plans
touched by writes_needed events}` and regenerate each
affected page. On `--rebuild`, regenerate all plan pages.

## Surfaces touched

- `crates/cli/src/cli/html.rs`:
  - New `render_plan_page(repo, stem, plan_events,
    reviews, subjects, status_info) -> String`.
  - New `plan_body_at_head(repo, stem, finalized: bool)
    -> Option<String>` helper (reads the right tracked
    path via `git show HEAD:<path>`).
  - In `build_site`: compute `affected_plans`, write
    `out_dir.join("plan/<stem>.html")` per affected plan
    (or every plan on full rebuild).
  - Wrap plan badges in `<a href="plan/<stem>.html">` in
    the index umbrella headers and the commit page's
    plan-line.
  - CSS for plan-page chrome (mostly reuse `.md` block
    styling).
- No core changes. No new deps.

## Tests

- `html_writes_plan_page_for_each_active_plan` — seed
  two active plans `foo` and `bar`; assert
  `plan/foo.html` and `plan/bar.html` exist.
- `html_writes_plan_page_for_finished_plans` — seed +
  finalize a plan; assert `plan/<stem>.html` exists and
  reads `.clank/finished/<stem>.md` at HEAD for its body.
- `html_plan_page_contains_events_for_that_plan_only` —
  two plans + one mixed `[foo,bar]` commit; the foo page
  contains the mixed commit row, the bar page does too,
  neither page contains the OTHER plan's intro row.
- `html_plan_page_renders_markdown_body` — plan body
  has a heading; assert the rendered page contains
  `<h1>…</h1>` from the markdown.
- `html_index_umbrella_links_to_plan_page` — assert the
  umbrella header for `foo` wraps its badge in
  `<a href="plan/foo.html">`.
- `html_commit_page_plan_line_links_to_plan_page` —
  assert the commit page's plan badge links to
  `../plan/<stem>.html`.
- `html_incremental_rewrites_affected_plan_pages` —
  first build with one plan, second build after a new
  commit on a different plan; assert ONLY the second
  plan's page got rewritten (the first plan's page
  retains its mtime).

## Out of scope

- Aggregate index of all plans (`plans/index.html`). The
  main `index.html` already lists active plans in its
  status header; a separate dashboard isn't needed yet.
- Per-plan diff aggregation ("combined diff for all
  commits attributed to this plan"). Nice-to-have, not
  urgent.
- Linking individual reviews from the plan-body section
  to their per-commit page. The timeline above already
  carries those links.
- A "current revision diff" view (current plan body vs
  previous revision). Skip for v1.
- Search / filter on the plan timeline. Cmd-F covers it
  for now.
