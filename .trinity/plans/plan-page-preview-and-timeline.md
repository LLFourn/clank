# Plan Page + Homepage UX

## Summary

Two pages, five changes:

**Plan landing (`/plan/{repo}/{stem_md}`):**

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

**Homepage (`/`):**

3. **Active plans only, sorted by recency.** The plans table today
   lists every plan in every watched repo, including `done`, in
   plan_id-lex order. Cut to active plans, sorted by most-recent
   activity timestamp (latest commit OR latest feedback file mtime,
   whichever is newer). Done plans stay reachable via direct URL or
   a "show done" toggle.

4. **"Waiting on" surfaces agents.** The current table has a
   `Waiting on` column (master / reviewers role) plus a free-prose
   `Description` column. Replace `Description` with `Who`, a chip
   list of the agent labels Trinity is waiting on (e.g. `codex`,
   `human`, or empty when role is `master`/`none`). The role chip
   stays; the prose disappears.

5. **Watched repos list with unwatch.** Repos are the primitive
   Trinity tracks, not plans. Surface them as a first-class section
   on the homepage with basename, canonical path, plan count, last
   activity. Each row has an "unwatch" button that deregisters the
   repo from the runtime, stops its filesystem watcher, and removes
   it from `~/.trinity/repos` — without touching anything inside the
   repo itself.

The plan-landing changes (1, 2) and the homepage changes (3, 4, 5)
are independent — they could ship in either order — but bundle into
one branch because they share the backend stride (new fields on
existing snapshots, one new endpoint family for repo management).

All scoped to the current plan/impl model; ship before
[[commit-centric-reviews]] (no model rewrite needed).

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

### Homepage shows everything in one undifferentiated list

The plans table mixes active and done plans (today: 4 of 7 rows are
already done) in plan_id-lex order. Done plans push active work down
the page; the order has no correlation with what you're likely
looking for.

The `Waiting on` column carries useful structure (role chip:
`master` / `reviewers`), but the `Description` column repeats the
same prose ("Plan was revised; awaiting re-review from codex.") for
every plan in the same state. The actual signal — *which agents* —
is buried in the prose. Operators reading the table want to scan
for their own label.

### Repos are invisible

Trinity watches repos. Plans are derived. Today the homepage shows
plans without ever showing repos, which means:

- No way to know what's being watched without grepping
  `~/.trinity/repos` or looking at the daemon logs.
- No way to stop watching a repo from the UI. Today you have to
  edit `~/.trinity/repos`, then restart the daemon. Friction big
  enough that orphan repos accumulate.

The "watched repos" set is the fundamental thing — plans flow from
it. The UI should treat it as a first-class section, not an
implementation detail.

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

### Homepage layout

```
┌─ Watched repos ─────────────────────────────────────┐
│ trinity    /Users/llfourn/src/trinity     4 plans   │
│            last activity 2 min ago        [unwatch] │
│ bdk-review /Users/llfourn/src/bdk-review  1 plan    │
│            last activity 3 hours ago      [unwatch] │
└─────────────────────────────────────────────────────┘

┌─ Active plans  [□ show done]  ──────────────────────┐
│ Plan                          State  Phase  Worktree │
│  Waiting on    Who                                   │
├──────────────────────────────────────────────────────┤
│ trinity/foo.md               active  impl   clean    │
│  [reviewers]   codex, human                          │
│ bdk-review/bar.md            active  plan   dirty    │
│  [master]      —                                     │
└──────────────────────────────────────────────────────┘
```

- Watched repos rendered as cards/rows above the plans table. One
  per registered repo, basename as the heading, canonical path as
  the muted subtitle.
- "Active plans" header carries a toggle for `show done`. Default
  off; persisted in `localStorage` so the choice survives a
  refresh.
- Plans sorted by most-recent-event timestamp descending. Done
  plans (when shown) interleave by the same key.
- The `Description` column is gone. `Waiting on` keeps the role
  chip; new `Who` column lists agent labels as small chips. When
  `agents` is empty, render an em-dash.

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

### Backend — homepage shape

`/api/plans` response keeps `plans` and `conflicts`, gains:

- `last_activity_ts: i64` on each plan row. Computed per plan as
  `max(latest_commit_time, latest_feedback_mtime, plan_intro_time)`.
  `latest_commit_time` = author-time of the newest commit in
  `plan.commit_order ∩ commits-attributed-to-this-plan`.
  `latest_feedback_mtime` = max `created_at` across `plan_feedback`
  + `impl_feedback` + `held_plan_feedback`. The fold is daemon-side
  so the frontend doesn't need to peek into per-plan internals.
- Each plan row already carries `waiting_on.agents`; the frontend
  consumes it directly. No backend change for the `Who` column.
- Server returns active+done as today. Filtering active-only is a
  frontend concern (the toggle should not require a refetch).
- Sorting: server returns plans in `last_activity_ts` descending so
  the homepage doesn't have to sort. Conflicts likewise.

New endpoint family: `/api/repos`.

- `GET /api/repos` →
  `{ repos: [{ basename, root, plan_count, last_activity_ts }] }`.
  Already-known data, served from a fresh runtime lock.
- `DELETE /api/repos/{basename}` → unregister the repo:
  1. Lock `Trinity`.
  2. Look up the canonical root for `basename`; 404 if absent.
  3. Remove from `Trinity.repos` and `Trinity.repo_basenames`.
  4. Drop the matching `notify_bridge` watcher handle from
     `AppState.watchers` / `AppState.watched_repos`. The handles
     are `JoinHandle`s today; aborting + awaiting is the cleanup
     contract.
  5. Rewrite `~/.trinity/repos` without this path.
  6. Push a `repo_unwatched` `LiveEvent` so the SPA's
     `EventStore.tick` fires and the homepage refetches.

The watcher-handle bookkeeping is the only non-trivial piece. Today
`AppState` has `watchers: Mutex<Vec<JoinHandle<()>>>` and
`watched_repos: Mutex<HashSet<PathBuf>>`. Need to associate handles
with canonical paths so removal can target one. Either change
`watchers` to `BTreeMap<PathBuf, JoinHandle<()>>`, or add a
`watcher_handles_by_repo` map alongside. The map form is the smaller
diff.

`Runtime::remove_repo(canonical: PathBuf) -> Result<RemoveOutcome,
RuntimeError>` owns the lock + state mutation; the HTTP handler
threads the watcher-handle cleanup around it. `RemoveOutcome`
distinguishes `Removed` from `NotPresent` so the handler can map to
404 vs 200.

### Frontend — homepage components

`PlansIndex` (in `frontend/src/api.rs`) gains `last_activity_ts: i64`
on each `PlanRow`. The existing `waiting_on.agents` is already
deserialized.

New `RepoRow` + `ReposIndex` types; new `fetch_repos()` helper:

```rust
pub struct RepoRow {
    pub basename: String,
    pub root: String,
    pub plan_count: u32,
    pub last_activity_ts: i64,
}
pub struct ReposIndex { pub repos: Vec<RepoRow> }
pub async fn fetch_repos() -> Result<ReposIndex, FetchError> { … }
pub async fn delete_repo(basename: String) -> Result<(), FetchError> { … }
```

New `<WatchedRepos/>` component renders above the plans table.
Each row has an `<UnwatchButton/>` that, on click, prompts for
confirmation (single in-row "are you sure?" — no modal dialog),
then calls `delete_repo`. On success, `EventStore.tick` already
fires from the daemon's `repo_unwatched` LiveEvent and the page
refetches; on failure surface the error inline.

`<Home/>` adds:

- A `show_done: RwSignal<bool>` initialized from
  `localStorage.getItem("show_done") == "true"`. Persist on change.
- Filter `plans` by `state == "active" || show_done.get()` before
  rendering.
- Drop the `Description` column. Keep the `Waiting on` (role) column;
  add a `Who` column that maps `waiting_on.agents` to chips:
  ```html
  <div class="waiting-who">
    {agents.iter().map(|a| view! { <span class="agent-chip">{a}</span> }).collect_view()}
  </div>
  ```
  When `agents` is empty, render `<span class="muted">"—"</span>`.

### Reactivity for repos

The watched-repos list is driven by a `LocalResource` keyed on
`EventStore.tick` (same pattern as `<Home/>` for plans). Server-side
the new `repo_unwatched` LiveEvent is broadcast through the SSE
pipeline, the SPA bumps `tick`, the resource re-fetches. Removing a
repo also means any plan rows from that repo vanish on the next
plans-refetch — which is the same tick, so the UI stays consistent.

## Phases

Three phases. They split cleanly between backend additions, repo
management (the only piece that adds new endpoints), and the
frontend changes that consume both.

### Phase 1 — Backend additions to existing endpoints

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
- `api_plans` adds `last_activity_ts` per plan row and returns
  rows sorted by it descending; conflicts likewise. Computed
  daemon-side from `max(latest_commit_time, latest_feedback_mtime,
  plan_intro_time)`.
- Snapshot/serialization tests cover: subject populated, body_html
  rendered, message_body parsed correctly for commits with and
  without an extended body, `last_activity_ts` correct for commit-only
  and feedback-only changes.

No frontend changes in this phase; existing UI still works (it
ignores the new fields).

### Phase 2 — Repo management endpoints

Files: `src/runtime.rs`, `src/server/mod.rs`, `src/server/http.rs`,
`src/server/notify_bridge.rs`, `tests/end_to_end.rs`.

- `AppState.watchers` shape changes from `Vec<JoinHandle<()>>` to
  `BTreeMap<PathBuf, JoinHandle<()>>` (or sibling map keyed on the
  canonical repo root). All existing insert sites updated.
- `Runtime::remove_repo(canonical) -> Result<RemoveOutcome, _>`
  drops the repo from `Trinity.repos` + `Trinity.repo_basenames`
  under one lock. Returns `Removed { plan_count }` or `NotPresent`.
- `~/.trinity/repos` rewrite helper in `src/server/mcp.rs` (today
  the `persist_repo_in_registry` function lives there). Add a
  matching `remove_repo_from_registry(&Path)`.
- `LiveEvent` gets a `repo_unwatched` kind; emit it from the
  remove path so SSE subscribers refetch.
- `GET /api/repos` and `DELETE /api/repos/{basename}` handlers in
  `src/server/http.rs`. The DELETE handler aborts the watcher
  handle, calls `Runtime::remove_repo`, calls
  `remove_repo_from_registry`, emits the LiveEvent.
- E2E tests:
  - `api_repos_lists_watched`: spawn daemon with two repos, GET
    `/api/repos` returns both with plan counts.
  - `delete_repo_removes_from_state_and_registry`: register repo,
    DELETE, verify subsequent `/api/plans` doesn't list its plans
    and `~/.trinity/repos` doesn't contain the path.
  - `delete_repo_404_on_unknown_basename`: DELETE against unknown
    basename returns 404.

No frontend changes in this phase either; tests verify the
endpoints in isolation.

### Phase 3 — Frontend: preview + accordion + homepage rework

Files: `frontend/src/api.rs`, `frontend/src/components/session_detail.rs`,
`frontend/src/components/plan_preview.rs` (new),
`frontend/src/components/timeline.rs`, `frontend/src/components/styles.css`
(or equivalent), `frontend/src/components/expanded_commit.rs` (new),
`frontend/src/components/home.rs`, `frontend/src/components/watched_repos.rs`
(new), `frontend/src/store.rs`.

- `PlanDetail` gains `plan_body_html` + `plan_body_truncated`.
  `PlanRow` gains `last_activity_ts`. `TimelineEvent::Commit*`
  variants gain `subject`. `CommitDiffPage` gains `subject` +
  `message_body`. New `RepoRow` / `ReposIndex` types plus
  `fetch_repos()` / `delete_repo()`. New `LiveEventKind` variant
  on the SSE store for `repo_unwatched`.
- `<PlanPreview/>` mounted in `<SessionDetail/>` above the
  timeline (matches the ASCII mockup); the latest-review card sits
  below the preview.
- `<TimelineRow/>` for commit variants becomes a clickable
  accordion (caret + subject); expand mounts
  `<ExpandedCommit plan_id sha />`.
- `<WatchedRepos/>` rendered above the plans table in `<Home/>`.
  Unwatch button asks "Are you sure?" inline (two-click confirm,
  no modal).
- Plans table:
  - Drop the `Description` column.
  - Add a `Who` column built from `waiting_on.agents`.
  - Filter to `state == "active"` unless `show_done` is true.
  - Persist `show_done` in `localStorage`.
  - Sort handled server-side; frontend just renders as received.
- CSS for `.plan-preview`, `.plan-preview-fade`,
  `.timeline-row.expanded`, accordion caret, `.watched-repos`,
  `.agent-chip`.
- Test plan: trunk build clean; visual check of preview
  collapse/expand, timeline accordion, watched-repos
  unwatch flow, show-done toggle persists across reload.

## Acceptance Criteria

### Plan landing page

1. Renders the current plan body at the top of the right column,
   capped at ~480px with a visible fade, with a "see full plan"
   button.
2. Clicking "see full plan" expands inline; clicking again
   collapses back. No page navigation.
3. The latest feedback targeting the current `review_target`
   renders as a single card under the preview; falls back to a
   muted "No reviews yet" when empty.
4. Each commit row in the timeline shows its kind, short SHA, and
   first-line subject (truncated with title hover for overflow).
5. Clicking a commit row expands an inline panel showing the full
   commit message and the structured diff. Second click collapses.
6. Expanded state on individual rows survives an SSE-driven
   rebuild (timeline re-renders but keyed rows keep their state).
7. No backend round-trip for the plan preview expand (HTML
   already in the initial response).
8. Backend round-trip on commit expand uses the existing
   `/api/plan/{repo}/{stem_md}/commit/{sha}` endpoint with the
   two added fields.

### Homepage

9. Plans table shows active plans only by default.
10. A `show done` toggle reveals done plans interleaved into the
    same table; toggle state persists in `localStorage`.
11. Plans are sorted by `last_activity_ts` descending (newest
    activity first), both with and without done shown.
12. The `Description` column is gone; a `Who` column lists
    `waiting_on.agents` as chips, em-dash when empty.
13. A `Watched repos` section above the table shows one row per
    registered repo with basename, canonical path, plan count, and
    last activity.
14. Each repo row has an "unwatch" button; clicking it prompts
    inline for confirmation, then removes the repo from runtime
    state, watcher set, and `~/.trinity/repos`.
15. After unwatch, the plans table refetches and the unwatched
    repo's plans disappear within one SSE tick — no full reload.

### Quality gates

16. `cargo fmt`, `cargo clippy --lib --tests -- -D warnings`,
    `cargo test -p trinity --lib --tests`, `trunk build` all pass.
17. New backend tests for: `last_activity_ts` computed from
    commits and feedback; `subject` populated; `plan_body_html`
    rendered; commit `message_body` parsed; `GET /api/repos`
    shape; `DELETE /api/repos/{basename}` happy path + 404.

## Non-Goals

- **Editing the plan from the page.** Read-only.
- **Inline review submission.** Reviews still go through the
  agent / file-drop workflow.
- **Diff folding controls in the inline accordion.** Use whatever
  the existing `<StructuredDiff/>` already provides; no new
  controls.
- **A separate "open commit in new tab" affordance.** The short SHA
  is already a link to the commit page; that's enough.
- **Adding repos from the UI.** `start_plan` from an agent inside
  a repo is the registration path. The homepage is read + remove
  only.
- **Sorting controls on the plans table.** Server-side
  `last_activity_ts desc` is the only order. If the user wants
  alphabetical they can ⌘F the page.
- **Bulk unwatch / multi-select.** One-at-a-time; orphan repos
  shouldn't be common.
- **Deleting the repo from disk.** Unwatch is purely a Trinity
  side-effect; the working tree is untouched.

## Open Questions

- **Preview height threshold.** 480px is a guess; the right answer
  depends on the plan font size and typical viewport. Pick during
  Phase 3 by eyeballing the first three trinity plans.
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
- **Unwatch confirmation UX.** Inline "Are you sure? [Confirm]
  [Cancel]" or a single-click with toast-to-undo? The plan picks
  inline-confirm because it's simpler and survives page-refresh
  amnesia. Toast-to-undo would need the daemon to defer the
  cleanup, which seems like the wrong axis to bend on.
- **Showing repos with zero plans.** If a repo is registered but
  has no committed plans, does it appear in the watched-repos
  list? **Yes** — that's exactly when you most want to see it
  (you registered it, nothing happened yet, you want to confirm
  Trinity sees it). `plan_count: 0` is fine.
