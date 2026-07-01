# html-open-loading-page

Make `clank html open` (and the TUI `o`) feel instant on the slow first
build: open the browser IMMEDIATELY on a beautiful, live-progress loading
page that auto-refreshes into the real report the moment the build finishes.
Supersedes the earlier `html-open-loading-stub` idea (same feature, now with
live progress + a designed page).

## Problem

Today `clank html open` builds the WHOLE site (every commit + plan page +
index) and only THEN launches the browser. First time (nothing built yet)
that's several seconds of nothing, then the browser pops. The TUI `o` spawns
it detached, so there's zero feedback in the meantime.

## Mechanism (approach A — open first, live loading page)

`clank html open`, when the resolved target page does NOT already exist (the
slow first-time / evicted-page case):

1. Write `.clank/html/_loading.html` — the designed loading page (below),
   starting in the "preparing…" state, with `<meta http-equiv="refresh"
   content="1">` (reloads itself every 1s — cadence confirmed fine).
2. Launch the browser on `_loading.html` NOW → the page appears ~instantly.
3. Run the full `build_site`, feeding progress to a **loading-page sink**
   that rewrites `_loading.html` with the live count (throttled to ~1s so it
   coincides with the refresh — no thrash).
4. On success: rewrite `_loading.html` to a redirect
   (`<meta http-equiv="refresh" content="0; url=<relative target>">`) → the
   next refresh jumps to the now-built real page.
5. On failure (or target still absent after build): rewrite `_loading.html`
   to the **error** state (below) and DROP the refresh meta so it stops.

When the target page ALREADY exists (fast incremental refresh), keep today's
build-then-open — no loading page, no flash.

`--print-path` mode never launches a browser, so it never writes a loading
page (unchanged).

## Single-page build — answer + decision

`render_commit_page` (html.rs:810) and `render_plan_page` (html.rs:989)
exist, so rendering ONE page is technically possible — but `build_site`
always does the full enumeration and there's no single-page entry point.
Per the ask, we deliberately keep the FULL build (the loading page is what
makes that acceptable) and do NOT add a single-page path here. (A
"render-the-target-first, build the rest behind" variant is a possible
future; out of scope.)

## Progress plumbing

`Progress` (html.rs:435) already tracks totals: `begin(label, total)` +
`tick(done)`, printed to the terminal. Reuse it:

- Add an optional **loading-page sink** to `Progress` (a path + the redirect
  target + a last-write timestamp). When set, `begin`/`tick`/`finish` also
  rewrite `_loading.html` (throttled to ~1s), and terminal output is
  suppressed for the detached TUI spawn (it's `--quiet` anyway).
- Unify the count into ONE bar: total = commit-page writes + plan-page writes
  (+ index), with cumulative `done` across the phases — so the page shows a
  single `07 / 16 pages`, not per-phase resets. (Small change to how the two
  `begin`/`tick` batches feed a shared counter.)
- Writes are atomic (temp file + rename) so a 1s refresh can never catch a
  half-written page (and self-corrects next tick regardless).

## The loading page — frontend design ("phosphor console")

Engaging the frontend-design lens. clank is git-native and terminal-native,
so the page reads as a **refined phosphor console watching the build happen
live** — intentional and distinctive, not a generic spinner or AI-slop
gradient. Fully self-contained (inline CSS, system monospace — file:// is
offline, no external fonts/assets), tiny (it's rewritten every second).

**Direction**
- **Palette (CSS vars):** near-black warm ground `--bg:#0b0b0d`; warm
  off-white `--fg:#e8e4d8`; muted `--dim:#6b6660`; a SINGLE bold accent —
  amber phosphor `--accent:#ffb454`; progress track `--track:#1c1a17`; error
  `--err:#f26d6d`. Set `background:#0b0b0d` inline on `<html>` +
  `<meta name="color-scheme" content="dark">` so the 1s reload NEVER
  white-flashes.
- **Type:** `ui-monospace, "SF Mono", "JetBrains Mono", "Cascadia Code",
  Menlo, monospace`. Large, tight-tracked count; dim small labels. The
  composition (not a fancy webfont) carries the distinctiveness.
- **Layout:** centered single column, generous negative space. A dim `clank`
  wordmark with a `▮` cursor block. A target line — dim label + accent value:
  `rendering  commit a1b2c3d` / `rendering  plan foo`. Then the hero: a big
  monospace **block progress bar** — a row of cells `▓▓▓▓▓▓▓░░░░░░░░` (filled
  = accent with a soft glow, empty = track) with `07 / 16 pages` large
  alongside. A braille spinner `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` + dim `building the site…`. A
  faint footer: "this page refreshes into your report automatically."
- **Motion (1s-loop aligned to the refresh so it reads continuous despite
  the reload):** spinner cycles its frames via CSS `steps()` over a 1s loop;
  the filled bar has a soft 1s ease-in-out glow pulse; an optional faint
  "scan" sweep across the bar. Because every animation is a clean 1s loop, a
  reload restarts it at frame 0 seamlessly.
- **Depth/texture (restrained):** a very low-opacity inline-SVG grain
  overlay; soft phosphor `text-shadow`/`box-shadow` in the accent on active
  elements; optional faint scanline gradient. Refined, not cheesy-CRT.
- **States:** *preparing* (spinner, indeterminate bar, "preparing…") →
  *building* (live `X / N` + filled bar) → *done* (brief `✓ ready` flash is
  optional; the redirect meta fires) → *error* (accent flips to `--err`,
  spinner → `✗`, "build failed — run `clank html` in a terminal for
  details", refresh meta removed).

The implementer builds the real HTML/CSS to realize this — the plan pins the
direction, palette, layout, motion, and the flash-free reload technique.

## Failure & edge cases

- **Build error / target-still-missing** → error state, refresh dropped
  (no infinite spinner). The loading page is the ONLY feedback for the
  detached TUI spawn, so this matters.
- **Watchdog:** the loading page also carries a JS fallback that, after ~60s
  of refreshing, switches itself to the error/"still building — check the
  terminal" state, in case the process died without writing the error.
- **Concurrent opens:** one shared `_loading.html`; last-open-wins is fine
  (rare). Note it; don't over-engineer per-open files.
- `_loading.html` lives under `.clank/html/` (already gitignored via the
  html output dir).

## Tests

- The loading-page HTML builder is a pure function: `render_loading_page(
  target_label, done, total, State)` → assert it contains the count, the
  target label, the accent/error markers per state, the refresh meta in
  building state and its ABSENCE in the error state, and the redirect url in
  the done state.
- Progress throttle: two ticks <1s apart rewrite the file at most once
  (pure/injectable clock).
- `clank html open` on a MISSING target writes `_loading.html` before the
  build and a redirect after (in-process, per the no-binary-spawn rule —
  drive `run`/the helper against a temp repo; assert file states, no browser
  launch under `--print-path`).
- Existing-target path: no `_loading.html` written (build-then-open kept).

## Acceptance criteria

- First-time `clank html open` / TUI `o`: browser shows the designed loading
  page within a moment, live `X / N` updates ~every 1s, then auto-redirects
  into the real report; no white flash on reload.
- Build failure surfaces on the loading page (not an infinite spinner).
- Existing pages keep the fast build-then-open (no loading page).
- Loading page is self-contained + offline (no external assets); clippy at
  baseline; tests pass.

## Deploy

After FINISHED: `cargo install --path crates/cli --force`.
