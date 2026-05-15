# Leptos SPA Frontend

## Summary

Replace `src/server/ui.rs` (raw maud HTML strings) with a Leptos SPA that talks to the existing daemon over JSON + SSE. The runtime, reducer, MCP surface, and HTTP API stay exactly as they are; this is a UI rewrite, not a daemon rewrite.

The daemon's `projection::*` layer (phase, waiting_on, gates, worktree status, attribution) already exposes everything the UI needs as pure functions. SSE already pushes change events. A reactive frontend is the natural shape: signals subscribe to SSE, re-fetch on event, render derived views. No htmx OOB acrobatics, no server-rendered HTML strings, no inline `<style>` per page.

**The Leptos SPA does NOT consume MCP tool responses.** It talks to a dedicated UI-only API surface (`/api/sessions/*`) that composes shapes the UI wants. See "Surface separation" below — that boundary is load-bearing.

## Why now, not a maud restore

The pre-rewrite UI lived in ~1900 LOC of maud strings + 600 LOC of CSS-in-Rust. Restoring it on the new core would be ~700 LOC of work that throws away cleanly once Leptos lands. The filesystem-truth core was designed for an SPA — pure projections, deterministic state digest, SSE on change. Leptos is the destination; jumping straight there avoids a maud detour.

## Aesthetic direction

**"Calm precision."** Trinity is a tool for thinking about code review, plan revisions, and commit attribution. The UI should feel like a well-edited technical journal — typographic hierarchy that breathes, generous whitespace, monospace for SHAs/diffs/code, sparing color used only for signal (verdict, phase state, waiting role). Not maximalist; not generic dashboard; closer to a refined documentation site with live data.

### Typography

| Use | Family | Source | Why |
|---|---|---|---|
| Display (h1, banners) | **Fraunces** | Google Fonts (variable, OFL) | Distinctive variable serif with soft/hard axis; sets editorial tone without being cute |
| Body / chrome | **General Sans** | Fontshare (Indian Type Foundry, free) | Modern grotesque, very legible at small sizes, slightly warmer than Helvetica/Inter |
| Monospace | **JetBrains Mono** | Google Fonts (OFL) | Industry-standard for code, has ligatures, opens up at small sizes |

All three self-hosted from `/static/fonts/`. No CDN dependency.

### Color tokens

CSS custom properties, dark-by-default with a light counterpart toggled by `prefers-color-scheme: light`.

```css
:root {
  --bg:           #0d1117;   /* deep blue-black; reads as paper at night */
  --surface:      #161b22;   /* cards, code blocks */
  --surface-2:    #1f2730;   /* hover state, inset wells */
  --border:       #30363d;
  --text:         #e6edf3;   /* warm off-white, not pure white */
  --text-muted:   #8b949e;
  --text-dim:     #6e7681;
  --accent:       #d2a8ff;   /* dusty violet — Trinity's one signature color */
  --approve:      #3fb950;   /* muted green */
  --request:      #f85149;   /* muted red */
  --pending:      #d29922;   /* amber for held / changes_requested */
  --mute:         #484f58;
}
```

The accent is used once per view as the "you are here" affordance — current SHA, active link, focused waiting_on role. Verdicts have their own colors; phase chips reuse approve/pending/mute.

### Motion

Subtle, signal-driven. Three rules:

1. **New events pulse, don't bounce.** When SSE delivers a new commit/review, the row's left border flashes the accent color for 600ms then fades. No scale transforms.
2. **Loading is a single skeleton.** No spinners. Lazy data slots show a 1px-wide animated gradient line (the underline trick) until they fill.
3. **Reductive transitions.** Toggling between sessions is an instant route swap. Opening details (`<details>` for files in a diff) uses the native browser animation.

### Layout

A two-column reading layout for session detail (sidebar with metadata + waiting_on + gates; main column with timeline + plan body + diffs). Single-column for the homepage. The header stays sticky on scroll and shows the session id + a compact waiting_on chip when scrolled past 100px.

Wide reading column (~720px max for prose; full-width for diffs). Generous margin top so the eye lands well.

## Architecture

```
                   +------------------+
                   |  Leptos SPA      |
                   |  (WASM)          |
                   |                  |
   +- SSE --------->|  reactive signals|
   |               +--------+---------+
   |                        |
   |                        | JSON HTTP
   |                        v
+--+--------------------------+
|  axum (current daemon)      |
|  GET  /events (SSE)         |
|  GET  /api/sessions         |
|  GET  /api/sessions/:id     |
|  GET  /api/sessions/:id/plan/:sha
|  GET  /api/sessions/:id/commit/:sha
|  GET  /api/diff?from=&to=&path=
|  POST /api/sessions/:id/done?repo=
|  POST /internal/tool_call   |  (MCP, unchanged)
+--+--------------------------+
   |
   v
+------------------+
|  Runtime         |
|  (in-memory)     |
+--+---------------+
   |
   v
+------------------+
|  rebuild_repo    |
|  (filesystem +   |
|   git, sans-IO   |
|   derive_state)  |
+------------------+
```

The daemon adds a dedicated UI JSON-API layer at `/api/*` built by a new `ui_response::*` module that composes the existing `projection::*` primitives + new diff endpoints. **This is not a wrapper around `mcp_response::*`** — the MCP and UI shapes are independent so neither bloats the other. The SPA bundle is served from `/` and `/static/*`.

## Multi-repo identity

Trinity tracks many repos; session ids are repo-scoped (two repos may both have a `foo` session). Identity is a `(repo, session_id)` pair everywhere — API, URLs, SSE filter, store keys.

**Repo identifier.** Each session response carries a `repo` field — URL-encoded absolute path of the repo root (the canonical form used as the in-memory key). Stable across requests; survives daemon restart. URL-encoded so it's safe in query strings.

**URL convention.** Multi-repo identity rides as a `?repo=<encoded>` query parameter, optional only when a single repo is registered (default selection). All session links emit the parameter:

- `/?repo=<path>` — homepage filtered to one repo (omit `?repo` to see all).
- `/sessions/:id?repo=<path>` — session detail.
- `/sessions/:id/plan/:sha?repo=<path>` — plan revision view.
- `/sessions/:id/commit/:sha?repo=<path>` — commit diff.

**Store keys.** Reactive resources key on `(repo, session_id, last_event_for_repo)`. The `EventStore` exposes `last_event_for_repo(repo) -> Memo<Option<LiveEvent>>` (filter the SSE stream by `repo`).

**Collisions.** When two repos both have session `foo`, the `?repo=` param disambiguates. The homepage's session table shows the repo name (last path segment) as a column when more than one repo is registered.

## Routes (client-side)

All paths serve the same `index.html` Leptos shell (see "axum routing order" below); the client router dispatches.

| Path | Component | Data sources |
|---|---|---|
| `/?repo=<path>` | `<Home/>` | `GET /api/sessions[?repo=<path>]` |
| `/sessions/:id?repo=<path>` | `<SessionDetail/>` | `GET /api/sessions/:id?repo=<path>` |
| `/sessions/:id/plan/:sha?repo=<path>` | `<PlanRevision/>` | `GET /api/sessions/:id/plan/:sha?repo=<path>` |
| `/sessions/:id/commit/:sha?repo=<path>` | `<CommitDiff/>` | `GET /api/sessions/:id/commit/:sha?repo=<path>` |
| `/sessions/:id/plan/:sha/diff?vs=:other&repo=<path>` | `<PlanDiff/>` | `GET /api/diff?from=:other&to=:sha&path=...&repo=<path>` |

Every component subscribes to a single global SSE store; when an event for "their" `(repo, session_id)` arrives, the resource is invalidated and refetched.

## axum routing order

Concrete `Router` shape so direct-navigation deep links (`/sessions/foo/plan/sha`) work without redirects:

1. `Router::new().nest("/api", api_router)` — JSON only. Returns 404 on unknown `/api/*` paths. Includes:
   - `GET /api/sessions[?repo=<path>]`
   - `GET /api/sessions/:id?repo=<path>`
   - `GET /api/sessions/:id/plan/:sha?repo=<path>`
   - `GET /api/sessions/:id/commit/:sha?repo=<path>`
   - `GET /api/diff?from=&to=&path=&repo=<path>`
   - `POST /api/sessions/:id/done?repo=<path>` — operator action to move plan to `plans/done/` (plain `std::fs::rename`; never `git mv`). Repo param required; returns `{ ok: true, new_plan_path }` or 404 if the session isn't found in that repo.
2. `.nest_service("/static", ServeDir::new("frontend/dist"))` — static assets (Wasm bundle, fonts, CSS).
3. `.route("/events", get(sse_handler))` — SSE.
4. `.route("/internal/tool_call", post(...))` — MCP backend, unchanged.
5. `.fallback(serve_index_html)` — every other GET serves `frontend/dist/index.html`. The Leptos router on the client side renders the right view from the URL.

No redirects on app paths. No collisions: anything that doesn't match `/api/*` or `/static/*` or `/events` or `/internal/*` ends up at the SPA shell, which routes client-side. The old top-level `POST /sessions/:id/done` route from `src/server/ui.rs` is removed in Phase 5; the SPA calls `POST /api/sessions/:id/done?repo=...` instead.

## Component model

Top-down:

- **`<App/>`** — `Router` + global `EventStore` provided via context.
- **`<Header/>`** — sticky, fades in chip + waiting badge when scrolled past 100px.
- **`<Home/>`** — `<SessionTable/>` for active sessions, `<SessionTable/>` for done sessions, `<ActivitySidebar/>` showing the last 40 SSE events with relative timestamps.
- **`<SessionTable/>`** — rows of `<SessionRow/>`. Each row: session id (link), phase chip, worktree-status chip, waiting_on chip, "last activity X ago".
- **`<SessionDetail/>`** —
  - Sidebar: `<MetaStrip/>` (base commit, repo, plan_path, worktree status, plan-gate, impl-gate), `<WaitingBanner/>` with full description + agents list.
  - Main: `<Timeline/>` (the unified one — Commit / Review / HeldFeedback rows), `<FeedbackList/>` (cards grouped by target SHA, collapsible), `<PrHintPanel/>` (when phase is implementing — squash command preview with copy-to-clipboard).
- **`<PlanRevision/>`** — `<MarkdownRenderer/>` for the plan body (server-rendered HTML from `pulldown-cmark`, sanitized with `ammonia`), `<FeedbackList/>` filtered to this SHA, prev/next nav links.
- **`<CommitDiff/>`** — `<StructuredDiff/>` (file list, fold-by-file `<details>`, line numbers, +/- coloring), `<FeedbackList/>` filtered to this SHA.
- **`<PlanDiff/>`** — `<StructuredDiff/>` for plan body between two revisions, with a header noting "Plan: from sha_a … to sha_b".
- **`<MarkdownRenderer/>`** — receives HTML string from server (already sanitized), wraps in `<article class="prose">` for typography.
- **`<StructuredDiff/>`** — server-side parsed `Vec<DiffFile>` JSON; client renders. Each file: `<details open>` with `<summary>` showing path + insertions/deletions stats; body is a `<table>` of line numbers + content with class for +/- coloring.
- **`<FeedbackCard/>`** — verdict pill, author, target SHA link, body rendered as markdown (sanitized).
- **`<Timeline/>`** — vertical list with colored left-edge markers. New events animate the marker pulse for 600ms.

## Reactive state + SSE integration

A single `EventStore` resource is created once at app startup:

```rust
let store = create_rw_signal(EventStore::default());
let _conn = create_local_resource(
    || (),
    move |_| {
        let store = store.clone();
        async move { connect_sse(store).await }
    },
);
provide_context(store);
```

`connect_sse` opens `EventSource` (browser API), parses each message, and writes the event into the store's `last_event` signal. Components that depend on a specific repo's data use `create_resource` keyed on `(repo, session_id, last_event_for_that_repo)`. When a new event arrives, the resource's key changes → re-fetch → reactive views update.

This means **no manual cache invalidation**. Adding a new component automatically gets live updates as long as it depends on the resource.

## Surface separation: agent vs UI

**Hard rule:** the web UI does not extend MCP tool shapes. The MCP surface (`get_context`, `list_sessions`, `wait_for_work`, …) is for agents — every field in those responses costs context-window tokens on every wake-up, forever. If the UI needs something richer, it gets its own endpoint.

This means:

- `get_context` is allowed to **shrink**, not grow. After `wait_for_work` lands, an agent that's been told `{work, locations}` rarely needs anything else. Target shape: `{ phase, plan_path, plan_worktree_status, waiting_on, review_target, latest_plan_revision, latest_implementation_revision }`. Drop `timeline`, `pr_hint`, full-history lists, raw `plan_feedback` / `impl_feedback` arrays, `review_gate` from the MCP response.
- Every MCP tool keeps a parallel HTTP endpoint (byte-identical JSON, current pattern). Those are still the agent surface.
- UI-specific endpoints live under their own namespace (this plan uses `/api/sessions/...`). They can be as rich as the UI needs — they're not part of the MCP contract, no agent ever reads them.
- If both surfaces really need the same data, expose a building block both reuse, but don't fold UI bulk into the MCP shape.

The slimming of `get_context` is a separate small plan/commit — flagged here so the Leptos work doesn't accidentally re-couple to it.

## Prerequisite primitives (already landed in `runtime-lock-boundaries`)

`9c40457` (Implement runtime lock boundary snapshots) shipped the building blocks this plan depends on. Use them as the foundation; do not duplicate or work around them:

- **`Runtime::snapshot_repo(repo) -> Result<RepoSnapshot, _>`** — clones an owned repo snapshot under the runtime mutex; caller does disk/git I/O after the lock is released.
- **`Runtime::snapshot_session(repo, sid) -> Result<Option<SessionSnapshotBundle>, _>`** — same shape for one session plus the repo-level indexes its projections need. `Ok(None)` means the session is not committed; map to 404.
- **`RepoSnapshot` / `SessionSnapshot` / `SessionSnapshotBundle`** (`src/runtime_snapshot.rs`) — owned mirrors of the runtime state. `SessionSnapshot` includes `body` so the UI session detail can render the plan content without a separate fetch.
- **`PlanStatusReader` trait + `DiskPlanStatusReader`** (`src/mcp_response.rs`) — abstraction over the one disk read needed for `plan_worktree_status`. UI builders that need worktree status take a `&impl PlanStatusReader` parameter so tests can drop in a fake/blocking reader; production wires `DiskPlanStatusReader`.

The new `ui_response::*` builders consume snapshots directly. They do NOT call `snapshot.to_repo_state()` to round-trip back into `RepoState` — that pattern exists in `mcp_response.rs` only because rewriting the projection helpers' `&RepoState` signatures was out of scope for the lock-boundary commit. For the UI builders being written fresh, prefer taking `&SessionSnapshotBundle` (and per-projection helpers that accept it) so the second clone falls out.

## Server-side additions

Minimal additions on top of the existing daemon. None of these reshape MCP tool responses — they are UI-only endpoints with UI-only field sets.

1. **`/api/*` JSON endpoints** (UI surface, not MCP). Build on the snapshot APIs above:
   - `GET /api/sessions[?repo=<path>]` → `[{ repo, session_id, plan_path, phase, worktree_status, waiting_on }]`. Index row, matches `list_sessions` but with the `repo` field for the multi-repo home page. Handler: `Runtime::snapshot_repo` → `ui_response::sessions_index(&snapshot, &DiskPlanStatusReader)`.
   - `GET /api/sessions/:id?repo=<path>` → **UI-only rich shape**: `{ session_id, phase, plan_path, plan_worktree_status, waiting_on, review_target, latest_plan_revision, latest_implementation_revision, plan_revisions: [...], implementation_commits: [...], plan_feedback: Vec<FeedbackEntry>, impl_feedback: Vec<FeedbackEntry>, timeline: [...], pr_hint }`. Handler: `Runtime::snapshot_session` → `ui_response::session_page(&snapshot, &DiskPlanStatusReader)`. Pure post-snapshot — no `read_repo` closures, no disk I/O under the runtime mutex.
   - `GET /api/sessions/:id/plan/:sha?repo=<path>` → `{ body_html, body_raw, plan_intro, plan_intro_parent, previous_sha, next_sha, feedback: Vec<FeedbackEntry> }`. Body is pre-rendered HTML via `pulldown-cmark` + `ammonia`. `feedback` is the entries targeting this SHA.
   - `GET /api/sessions/:id/commit/:sha?repo=<path>` → `{ diff_files: Vec<DiffFile>, feedback: Vec<FeedbackEntry> }`. DiffFile parsed from `git show` output.
   - `GET /api/diff?from=&to=&path=&repo=<path>` → `{ diff_files }`. For plan-rev-vs-rev.

   Note: `/api/sessions/:id` deliberately does NOT call `get_context_response` — that's the MCP tool's shape. The new `ui_response::session_page` builder shares the `projection::*` primitives but lives in its own module so changes to one don't ripple to the other.

2. **Extended feedback shape (UI surface only).** The MCP `plan_feedback` / `impl_feedback` arrays continue to emit `{ target_sha, author, verdict }` (and probably get dropped from `get_context` entirely in the slim-down). The UI's `FeedbackEntry` is richer:

   ```json
   {
     "target_sha": "abc...",
     "author": "alice",
     "verdict": "approve",
     "body_raw": "APPROVE\n\nlgtm because...",
     "body_html": "<p>lgtm because...</p>",
     "path": ".trinity/feedback/foo/plan/abc.../alice.md",
     "created_at": 1715666400
   }
   ```

   `body_raw` is the file content verbatim; `body_html` strips the marker line and renders the rest via `pulldown-cmark` + `ammonia`. Phase-1 acceptance: every `/api/sessions/:id` response with feedback includes these fields. Held feedback gets the same extended shape (just no `target_sha`). The MCP-side `feedback_entries` builder is untouched.

3. **`src/diff_parser.rs`**. Port the deleted parser from `a2d7b5d^:src/daemon/diff_parser.rs`. Pure Rust, no IO. Tests: ~10 cases covering add/modify/delete/rename/binary/multi-hunk.

4. **Static-file route**. axum `tower-http`'s `ServeDir` mounted at `/static`. Fallback route serves `frontend/dist/index.html` (see "axum routing order" above).

5. **`Feedback.created_at: i64`**. File mtime, populated in `git_io::collect_feedback_files`. Lets the client sort feedback chronologically. Threaded into `DiskSnapshot::FeedbackBlob` (currently it only carries `body`).

## Workspace structure

Add a second crate to the workspace for the Leptos client:

```
trinity/
├── Cargo.toml            # workspace
├── src/                  # existing daemon (binary `trinity`)
└── frontend/
    ├── Cargo.toml        # cdylib + bin (CSR)
    ├── index.html
    ├── style.css
    ├── public/
    │   └── fonts/
    └── src/
        ├── main.rs
        ├── api.rs        # typed fetch wrappers
        ├── store.rs      # EventStore + SSE connection
        ├── components/
        │   ├── home.rs
        │   ├── session_detail.rs
        │   ├── plan_revision.rs
        │   ├── commit_diff.rs
        │   ├── timeline.rs
        │   ├── feedback_card.rs
        │   ├── structured_diff.rs
        │   └── markdown.rs
        └── route.rs
```

Build via `trunk` (simpler than cargo-leptos for CSR-only). `trunk build --release --public-url /static/` produces `frontend/dist/` with all asset references in `index.html` (Wasm, JS shim, CSS, fonts) prefixed by `/static/`. The daemon then mounts `ServeDir::new("frontend/dist")` at `/static`, so `<script>` and `<link>` tags resolve correctly. The fallback handler serves `frontend/dist/index.html` for app paths; the embedded `/static/...` references load the actual assets via the ServeDir mount.

Concretely, commit a `frontend/Trunk.toml`:

```toml
[build]
public_url = "/static/"
release = true
```

So plain `trunk build --release` (no flags) produces the right output. CI and the `xtask` invoke this command.

**Toolchain.** Trunk and the `wasm32-unknown-unknown` target aren't in a stock Rust toolchain. Bootstrap requirements:

1. `rustup target add wasm32-unknown-unknown`
2. Install trunk: `cargo install --locked trunk` (or `cargo binstall trunk` for a prebuilt). Pin the version in a top-level `rust-toolchain.toml` for the wasm target + a `.tool-versions` or README line for trunk.

CI runs the same two steps before `trunk build --release`. An `xtask` crate (`cargo xtask build-frontend`) wraps both for one-command local builds.

## Phases

Five commits, each green-buildable, each landing a usable slice.

### Phase 1 — JSON API + frontend crate scaffold (1 commit)

- Add `src/ui_response.rs` module with `sessions_index(&RepoSnapshot, &impl PlanStatusReader) -> Value` and `session_page(&SessionSnapshotBundle, &impl PlanStatusReader) -> Value`. Built directly on `projection::*` primitives + the snapshot types from `runtime-lock-boundaries`. No `read_repo` closures, no disk I/O under the runtime mutex. Test seam mirrors `mcp_response.rs`: `*_with_status_reader` for tests, default wrappers bind `DiskPlanStatusReader`.
- Add `/api/sessions`, `/api/sessions/:id`, `/api/sessions/:id/plan/:sha`, `/api/sessions/:id/commit/:sha` routes calling into `ui_response::*`. Shapes intentionally diverge from `mcp_response.rs` (richer — body, timeline, feedback bodies, pr_hint) per the surface-separation rule.
- Restore `src/diff_parser.rs` from `a2d7b5d^` with its unit tests.
- Pre-render markdown via `pulldown-cmark` + sanitize via `ammonia` on the server (already deps).
- Add `Feedback.created_at` to the state model; populate via `fs::metadata`.
- Add `frontend/` crate skeleton with a `<Home/>` that fetches `/api/sessions` and renders a basic table.
- Daemon serves `/static/*` and `/` (Leptos shell).
- Acceptance: `trunk build` succeeds; loading `/` in a browser shows the session list driven by the API. `rg "snapshot.to_repo_state" src/ui_response.rs` is empty — the new builders take snapshot types directly without round-tripping.

### Phase 2 — Session detail + timeline + feedback cards (1 commit)

- `<SessionDetail/>` with sidebar + main column.
- `<Timeline/>` rendering Commit / Review / HeldFeedback events with proper marker styling.
- `<FeedbackCard/>` with verdict pill + author + body markdown.
- `<MetaStrip/>` + `<WaitingBanner/>` + review-gate chips.
- Acceptance: `/sessions/filesystem-truth-rewrite` shows all the same info as the current page, but with proper typography and a real two-column layout.

### Phase 3 — Plan revision + commit diff + plan-rev-vs-rev diff (1 commit)

- `<PlanRevision/>` with rendered markdown, prev/next nav, feedback filtered to this SHA.
- `<CommitDiff/>` with structured `<details>`-per-file + line numbers + coloring.
- `<PlanDiff/>` with the same structured-diff component over plan-body diffs.
- Acceptance: every link in the timeline opens a rendered, readable view. A 1000-line diff is browsable (file list at top, expand-on-click).

### Phase 4 — SSE-driven live updates (1 commit)

- `EventStore` + `connect_sse` glue.
- Every `create_resource` keyed on `(target, last_event_for_target)`.
- Marker-pulse animation on row insertion.
- Activity sidebar on the homepage subscribes to the global event stream.
- Acceptance: commit something in a watched repo; without refreshing the browser, the timeline grows a new row with a brief accent pulse, and the activity sidebar gets a new entry at the top.

### Phase 5 — Typography, color, polish (1 commit)

- Self-host the three fonts under `/static/fonts/`.
- Final CSS pass with the color tokens above and prefers-color-scheme: light counterpart.
- Sticky header behavior.
- Copy-to-clipboard on `pr_hint` commands.
- Remove the old `src/server/ui.rs`. The `home`, `session_detail`, `plan_revision_view`, `commit_diff_view`, and `move_to_done` handlers go away entirely; the paths they served are picked up by the SPA fallback (for GETs) and by `POST /api/sessions/:id/done` (for the done action). **No redirects** — deep-link navigation must work without an intermediate hop.
- Delete the inline-style chime infrastructure; reimplement in `frontend/src/store.rs` as a Web Audio call when an event arrives.
- Acceptance: visit `/` in a fresh browser tab and the result is visibly distinct from the maud version — better typography, calmer color, no white-on-white, and instantly responsive to live events.

## Acceptance criteria

- `frontend/` crate builds clean via `trunk build --release` after `rustup target add wasm32-unknown-unknown` and `cargo install trunk` (or `cargo binstall trunk`). No system deps beyond the Rust toolchain + these two well-known bootstrap steps; documented in README and the `xtask`.
- Daemon serves the SPA bundle from `/` via fallback to `frontend/dist/index.html`; assets from `/static/*`; JSON API from `/api/*`; SSE from `/events`.
- No **authored** JS or TS outside `frontend/index.html` (the Leptos bootstrap shell). Trunk's generated wasm-bindgen glue counts as build output, not authored source.
- `src/server/ui.rs` is deleted by the end of Phase 5.
- No inline `<style>` strings in Rust source after Phase 5.
- Every page from the gap list in the (deleted) `frontend-restore.md` has a Leptos component.
- SSE-driven live update on every page: dropping a feedback file or making a commit visibly updates the open page without refresh within one debounce window.
- Typography uses the three named fonts; no system font fallback in the prose styles.
- One signature accent color used consistently for "current focus" affordance.

## Non-goals

- No SSR. Pure CSR via trunk. The daemon serves JSON; the client renders.
- No Tauri / desktop bundling. Browser-only.
- No state-sync framework beyond plain `create_resource`. No Redux/Zustand analog.
- No GraphQL. JSON over HTTP.
- No icon library. SVG inline only where unavoidable (verdict markers, prev/next arrows).
- No CSS framework. Hand-written CSS with custom properties.
- No service worker / offline mode.
- No comment/feedback submission form. Reviewers continue to write `.trinity/feedback/*` files directly.
- No new auth model.

## Risks

- **WASM bundle size.** Leptos + a structured-diff renderer + markdown styles can push the bundle to 500KB+. Mitigation: `wasm-opt`, drop unused features, accept the gzipped size.
- **Font loading flash.** Self-hosted fonts may FOIT/FOUT. Mitigation: `font-display: swap` + size-adjust descriptors to match metrics.
- **SSE reconnection.** Browsers reconnect EventSource automatically on disconnect, but reconnection storms during daemon restart need backoff. Mitigation: explicit exponential backoff in `connect_sse`.
- **Markdown injection.** Plans are author-controlled, but the author may be an LLM. Mitigation: `ammonia` defaults block scripts and most attributes; configure to allow a safe subset (headings, lists, code, links, tables, emphasis).
- **Live activity flood.** A noisy repo could push events faster than the UI can re-render. Mitigation: debounce SSE-triggered refetches at 250ms in the store; cap activity-sidebar updates at one per second.
- **Static fonts in repo.** Hosting fonts in git adds ~500KB to the repo. Acceptable — the alternative (CDN) breaks offline dev.

## Implementation notes

- Use `leptos_router` for client-side routing, `leptos_meta` for `<title>` updates per route.
- Keep components in single files; prefer many small components over fewer large ones.
- Type the API responses with `serde::Deserialize` structs in `frontend/src/api.rs`; share the shape definitions with the daemon by extracting them to a tiny `trinity-api` crate in the workspace, OR just duplicate them with comments noting they must stay in sync. (Defer the shared-types crate until Phase 3 when the friction shows.)
- The `EventStore` should expose typed accessors: `store.last_for_session(id) -> Memo<Option<LiveEvent>>`, so components don't reach into raw signal state.
- CSS lives in one file (`frontend/style.css`); no CSS-in-Rust, no styled-components analog. Discipline beats tooling.

## Deferred from the initial implementation

All five phases shipped, but a handful of plan items did not make it into the initial impl chain. They are intentional gaps, tracked here so the next reviewer / implementer doesn't think the plan is fully delivered:

- **Self-hosted fonts.** Fraunces / General Sans / JetBrains Mono are referenced as aspirational; `frontend/style.css` falls back to system fonts. Vendoring the woff2 binaries under `frontend/public/fonts/` (~500KB) + adding `@font-face` declarations is a mechanical follow-up. Auto-mode blocked the CDN download during initial impl.
- **Scroll-aware sticky header chip.** Section "Layout" promises the header shows the session id + waiting chip once scrolled past 100px. Phase 5 ships only the static sticky behaviour; the on-scroll chip swap is unwritten.
- **Active vs done split on the homepage.** Section "Component model" describes two `<SessionTable/>` blocks (active above, done collapsed below). Phase 1 / 2 ship a single table that includes done sessions inline.
- **`?repo=` multi-repo URL plumbing.** Section "Multi-repo identity" specifies that every route emits `?repo=<encoded path>` so multi-repo deployments deep-link cleanly. The current implementation drops `repo` from frontend URLs (the API still accepts it; the homepage falls back to "all watched repos"). Single-repo deployments aren't affected. Re-instate when a second watched repo regularly hosts a same-named session.
- **SSE reconnect backoff.** The plan's Risks section called this out; the current `connect_sse` relies on the browser's default reconnect behaviour. For a daemon-restart flurry that's fine; under sustained outages it can hammer. Add explicit exponential backoff if it surfaces.

## Open questions

- **Path of `index.html` in dev vs prod.** Trunk serves `dist/index.html` on its own port during dev; daemon serves the same file from `/` in prod. Need to configure CORS or a trunk proxy for dev → daemon API calls.
- **Bundle hash / cache headers.** Production should serve immutable assets with content-hash filenames. `trunk` supports this; verify the daemon sets cache headers correctly.
- **Light mode default vs dark default.** Defaulting to dark matches developer expectation; auto-toggle via `prefers-color-scheme` is the right move. Provide a manual override in the header via local storage.
