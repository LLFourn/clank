# Plan Page: Preview + Timeline Expandable Commits

## Summary

Two changes to `/plan/{repo}/{stem_md}`:

1. **Plan preview.** Render the current plan markdown directly on the
   landing page, faded out and clipped after a fixed height with a
   "see more" toggle that expands to full body. Today the plan body
   is only visible via the revision route — readers click through to
   read the plan they're meant to be reviewing.

2. **Timeline commit subjects + per-row expand.** Each commit row in
   the timeline currently shows kind ("Plan revision" / "Implementation"
   / "Plan + impl") plus a short SHA. Add the commit subject (first
   line of the message) so rows are skimmable, and make each row an
   accordion that expands to the full commit (message body + diff)
   without leaving the page.

Both improvements stay scoped to the current plan/impl model; they
ship before [[commit-centric-reviews]] (no model rewrite needed).

## Problem

### Plan preview missing

Landing page sections today, top-to-bottom: header (PlanId + state),
waiting banner, sidebar (meta-strip + PR hint), timeline, plan
feedback, impl feedback. The plan body itself — the thing every
reviewer and master arrived to read — is not on the page. To see it,
you click into `/plan/.../revision/{sha}`, which renders just the
body and feedback for one revision.

Effect: anyone landing on a plan to do or read about review work has
to make a second click before they know what the plan actually says.

### Timeline rows too thin

A timeline row today looks like:

```
●  Plan revision    [d3251cf]
●  Plan review APPROVE  codex  on d3251cf
●  Implementation   [536692a]
```

Two failure modes:

- **Skim fails.** "What did d3251cf change?" — no signal beyond the
  kind. The commit subject (`Fix SSE catch-up and feedback rendering`)
  is the cheapest disambiguator git already produces.
- **Drill-down requires navigation.** Clicking through to
  `/plan/.../commit/{sha}` works but the page swap loses your
  scroll position in the timeline and forces a re-render of everything
  else.

## Target Model

### Landing page additions

```
┌─ header ────────────────────────────────────┐
│ trinity/foo.md   [active]                   │
├─ waiting banner ─────────────────────────────┤
│                                              │
├─ sidebar ───────┬─ Plan ────────────────────┤
│ meta-strip      │ <rendered plan markdown>  │
│ PR hint card    │ … (fades out, ~600px)     │
│                 │ [see full plan]           │
│                 ├─ Latest review ───────────┤
│                 │ [verdict] author · target │
│                 │ <rendered body>           │
│                 ├─ Timeline ────────────────┤
│                 │ ▸ Plan revision   d3251cf │
│                 │   Fix SSE catch-up        │
│                 │ ▸ Plan review APPROVE     │
│                 │   codex   on d3251cf      │
│                 │ ▸ Implementation 536692a  │
│                 │   Live click-through      │
│                 ├─ All feedback ────────────┤
│                 │ … (chronological cards)   │
└─────────────────┴───────────────────────────┘
```

### Plan preview semantics

- Renders the current plan body (the body at HEAD's blob for the
  plan file — same body the revision route would produce for
  `latest_plan_revision`).
- Always rendered HTML; no on-demand fetch — the body's already in
  the snapshot the daemon serves to `plan_page`.
- Collapsed by default to a fixed pixel height with a CSS mask-image
  fade at the bottom. A "see full plan" toggle removes the height
  cap and the fade. No animation framework — pure CSS height
  transition is plenty.
- "Open as page" link next to the toggle goes to
  `/plan/{repo}/{stem_md}/revision/{latest_plan_sha}` for users who
  want the focused single-revision view.

### Latest review

A single feedback card immediately under the plan preview, surfacing
the most recent feedback targeting the current `review_target.commit_sha`.
Falls back to "No reviews yet" when there are none. The full feedback
section moves further down the page and behaves as it does today
(all cards, all phases, chronological).

The "latest review" card matches the existing `FeedbackCard` styling.
No new component needed; just `feedback.first()` after sorting by
`(target_sha == review_target.commit_sha, created_at desc)`.

### Timeline rows

```
▸  Plan revision   d3251cf   Fix SSE catch-up and feedback rendering
▸  Plan review APPROVE   codex   on d3251cf
▸  Implementation  536692a   Live click-through session timeline
```

- Each commit row gets a subject column. Reviews and held-feedback
  rows are unchanged.
- A subtle disclosure caret on the left of commit rows toggles an
  inline panel below the row carrying the full commit. Caret closed
  by default.
- Expanded panel content: commit message body (everything after the
  subject) plus the structured diff. Same rendering as the existing
  commit-detail route, just inlined.
- Subject text is the disclosure trigger (along with the caret) — the
  whole row is clickable; the short-SHA `<code>` remains a link
  to the dedicated commit page for users who want the full URL.

## Design

### Backend — `PlanDetail` additions

Two new fields on `/api/plan/{repo}/{stem_md}`:

- `plan_body_html: String` — rendered HTML of the current plan body
  (the body from `Plan.body` in the snapshot, fed through
  `ui_response::render_markdown`). The raw markdown can stay
  daemon-side; the frontend only needs HTML.
- `plan_body_truncated: bool` — set when the body exceeds ~4000
  characters (rough threshold so the SPA knows whether the "see
  full" toggle should appear at all). Server-side hint, not a
  truncation — the full body is always sent.

`TimelineEvent::Commit*` variants gain `subject: String`. The
subject is the first line of the commit message (`git log -1
--format=%s <sha>`). Per-snapshot the daemon already iterates
commits to build the timeline; subjects come from one extra git
invocation per snapshot — or batched as `git log --first-parent
--reverse --format=%H%x00%s` so it's one process spawn per repo
rebuild instead of per-commit.

The batched form fits the existing `commit_order` machinery in
`git_io::list_first_parent_commits`. The subject map gets stored
once per rebuild on `RepoState` (or `RepoSnapshot`) as
`commit_subjects: BTreeMap<CommitSha, String>` and merged into
timeline events at projection time. No per-request git calls.

### Backend — expanded commit data

The accordion fetches `/api/plan/{repo}/{stem_md}/commit/{sha}` —
which already exists, returns `{repo, plan_id, slug, commit_sha,
diff_files, feedback}`. Two adjustments:

- Add `subject: String` and `message_body: String` to the commit
  response (subject is the first line, message_body is everything
  after the first blank line). `git_io::show_commit` already
  returns the full commit text; parsing those two pieces out is one
  helper.
- The accordion only needs the diff once per expand; the response
  is small enough to fetch on demand without paging.

### Frontend — `<PlanPreview/>` component

New component under `frontend/src/components/plan_preview.rs`. Props:
`body_html: String`, `truncated: bool`, `revision_link: String`. State:
local `expanded: RwSignal<bool>` initialized to `false`.

```html
<section class="plan-preview" class:expanded={...}>
  <div class="plan-preview-body" inner_html={body_html} />
  <Show when=truncated>
    <div class="plan-preview-fade" />
    <div class="plan-preview-actions">
      <button on:click=toggle>{label}</button>
      <a href=revision_link class="muted">"Open as page"</a>
    </div>
  </Show>
</section>
```

CSS does the heavy lifting:

- `.plan-preview-body` has `max-height: 480px; overflow: hidden;
  transition: max-height 200ms ease-out;`.
- `.plan-preview.expanded .plan-preview-body { max-height: none; }`.
- `.plan-preview-fade` is a `linear-gradient(transparent → var(--bg))`
  overlay, hidden when `.expanded`.

### Frontend — `<TimelineRow/>` accordion

`TimelineRow` currently match-renders by variant. Add a `subject`
field to commit variants (post-API change) and a disclosure caret.

Per-row state: `expanded: RwSignal<bool>` default false. When
expanded, render an `<ExpandedCommit/>` panel that lazily mounts
`<LocalResource>` against `fetch_commit_diff(plan_id, sha)` — the
existing helper. On first expand the panel shows "Loading…" then
the diff; subsequent toggles re-use the resource (LocalResource
caches per dep).

Component layout:

```
┌─ row (clickable) ─────────────────────────────┐
│ ▸  Plan revision  d3251cf  Fix SSE catch-up… │
└───────────────────────────────────────────────┘
   ┌─ expanded panel (when open) ───────────────┐
   │ <pre>{message_body}</pre>                  │
   │ <StructuredDiff files={diff_files} />      │
   └────────────────────────────────────────────┘
```

The existing `<StructuredDiff/>` from `plan_diff.rs` /
`commit_diff.rs` already renders `Vec<FileDiff>`; reuse it.

### Subject parsing edge cases

- Empty subject — git won't produce this for non-empty commits;
  fall back to `"(no message)"`.
- Multi-line subjects — git's `%s` format expander already returns
  only the first line.
- Very long subjects — let CSS truncate with `text-overflow:
  ellipsis` and a `title` attribute carrying the full subject so
  hover shows the rest.
- Unicode — already handled; `String` round-trip is fine.

### Reactivity

The plan preview re-renders on `EventStore.tick` (already wired via
the parent's `LocalResource`). New commits push a `repo_rebuilt`
event; the SPA refetches the plan detail; the new HTML lands and
re-renders. Expanded state on the preview AND on individual timeline
rows is per-component-instance and survives refetch as long as the
component's identity in the DOM doesn't change. The `<For
key=timeline_key>` in `<Timeline/>` already keys commit rows by
`commit:{sha}` so per-row expanded state survives unrelated rebuilds.

The plan-preview `expanded` signal survives because its parent
component (`<SessionDetail/>`) re-uses the same DOM subtree on
refetch — only the `body_html` prop changes.

## Phases

Two phases. Both are small; the split is for review cadence, not
size.

### Phase 1 — Backend: subjects + plan body HTML

Files: `src/git_io.rs`, `src/repo_state.rs`, `src/runtime_snapshot.rs`,
`src/projection.rs`, `src/ui_response.rs`, `src/mcp_response.rs`,
`src/server/http.rs`.

- `list_first_parent_commits` (or its sibling) returns
  `(CommitSha, String)` pairs via `--format=%H%x00%s`. Parse into
  a `commit_subjects` BTreeMap stored on `RepoState`.
- `RepoSnapshot` and `PlanSnapshotBundle` carry the subjects map.
- Timeline projection threads subjects into `Commit{Plan,Impl,Mixed}`
  events. The daemon's `TimelineEvent` enum (in `repo_state.rs`)
  gains a `subject` field; projection populates it.
- `api_plan_detail` response gains `plan_body_html` (call
  `render_markdown(&snapshot.plan.body)`) and `plan_body_truncated`
  (computed against the raw length).
- `api_commit_diff` response gains `subject` and `message_body`
  parsed from `show_commit` output (split on the first blank line
  after the header block).
- Snapshot/serialization tests cover: subject populated, body_html
  rendered, message_body parsed correctly for commits with and
  without an extended body.

No frontend changes in this phase; existing UI still works (it
ignores the new fields).

### Phase 2 — Frontend: preview + accordion

Files: `frontend/src/api.rs`, `frontend/src/components/session_detail.rs`,
`frontend/src/components/plan_preview.rs` (new),
`frontend/src/components/timeline.rs`, `frontend/src/components/styles.css`
(or equivalent), `frontend/src/components/expanded_commit.rs` (new).

- `PlanDetail` gains `plan_body_html: String` and
  `plan_body_truncated: bool`.
- `TimelineEvent::Commit{Plan,Impl,Mixed}` variants gain
  `subject: String`.
- `CommitDiffPage` gains `subject` and `message_body` (already
  reused by `<CommitDiff/>` route — the existing page also wants
  the subject; add it there too).
- `<PlanPreview/>` mounted in `<SessionDetail/>` between meta-strip
  and timeline (or above timeline depending on layout decision —
  the ASCII mockup puts it above; the implementation should match
  the user's preference once we see it).
- `<TimelineRow/>` for commit variants becomes a button that toggles
  `expanded`. When expanded, mount `<ExpandedCommit plan_id sha />`
  which uses the existing `fetch_commit_diff` and renders message +
  diff inline.
- CSS for `.plan-preview`, `.plan-preview-fade`,
  `.timeline-row.expanded`, accordion caret animation.
- Test plan: trunk build clean; visual check of preview collapse +
  expand and timeline row expand/collapse.

## Acceptance Criteria

1. Landing page renders the current plan body at the top of the
   right column, capped at ~480px with a visible fade, with a "see
   full plan" button.
2. Clicking "see full plan" expands inline; clicking again collapses
   back. No page navigation.
3. The latest feedback targeting the current `review_target` renders
   as a single card under the preview; falls back to a muted "No
   reviews yet" when empty.
4. Each commit row in the timeline shows its kind, short SHA, and
   first-line subject (truncated with title hover for overflow).
5. Clicking a commit row expands an inline panel showing the full
   commit message and the structured diff. Second click collapses.
6. Expanded state on individual rows survives an SSE-driven rebuild
   (timeline re-renders but keyed rows keep their state).
7. No backend round-trip for the plan preview expand (HTML already
   in the initial response).
8. Backend round-trip on commit expand uses the existing
   `/api/plan/{repo}/{stem_md}/commit/{sha}` endpoint with the two
   added fields.
9. `cargo fmt`, `cargo clippy --lib --tests -- -D warnings`,
   `cargo test -p trinity --lib --tests`, `trunk build` all pass.

## Non-Goals

- **Editing the plan from the page.** Read-only.
- **Inline review submission.** Reviews still go through the
  agent / file-drop workflow.
- **Diff folding controls in the inline accordion.** Use whatever
  the existing `<StructuredDiff/>` already provides; no new
  controls.
- **A separate "open commit in new tab" affordance.** The short SHA
  is already a link to the commit page; that's enough.

## Open Questions

- **Preview height threshold.** 480px is a guess; the right answer
  depends on the plan font size and typical viewport. Pick during
  Phase 2 by eyeballing the first three trinity plans.
- **Truncation flag heuristic.** ~4000 characters covers "more than
  one screen of text"; might want to base it on rendered line count
  instead. The flag is a hint, not a hard rule — over-showing the
  toggle when the plan is just barely over is harmless.
- **Where to put the "latest review" card.** Above or below the plan
  preview? Argument for above: it's the action signal. Argument for
  below: you read the plan first, then the review. Default to
  **below**; revisit if it feels wrong in practice.
- **Should the timeline accordion be the default UX, or controlled
  by a per-row "expand" affordance?** The plan above goes with
  per-row caret. An alternative is "first row always expanded" —
  but that fights with `<For key>` reactivity since the "first
  row" changes as new commits land. Stick with explicit caret.
