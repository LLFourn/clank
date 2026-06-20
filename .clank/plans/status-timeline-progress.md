# status-timeline-progress

Two timeline affordances: (1) render a commit's review feedback ABOVE the
commit (in `clank log` oneline AND `status --tui`), so the display
respects time order; (2) in `status --tui`, show live in-progress
indicators — an animated spinner where a reviewer's verdict tick will go
while their review is pending, and a "master is working" row for the
commit master is currently producing.

## Why

Both views are newest-at-top (git-log convention; `clank log` reverses to
newest-first at log.rs:65, the TUI the same). A review happens AFTER its
commit, so in a newest-first list it belongs ABOVE the commit — today it
renders below, which reads backwards in time. And the TUI shows nothing
for work in flight: a pending reviewer is just an absent tick, and master
producing the next commit is invisible.

## Milestone 1 — feedback above the commit (both views)

- In `oneline_rows` (log.rs ~365–382), emit a commit's `Review` rows
  BEFORE the `Commit` row instead of after. Since callers pass events
  newest-first, this puts each review above its commit. Among multiple
  reviews of one commit, order newest-first if a timestamp is available;
  otherwise keep the current deterministic order (note which).
- Pure change; `oneline_plain_lines`, the TUI log renderer, and `clank
  log` all just iterate the Vec, so both views update from the one edit.
- Update the existing `oneline_rows` tests for the new order.

## Milestone 2 — live in-progress indicators (status --tui only)

Driven by the active plan's `WaitingOn` (already on the snapshot), NOT by
`log_rows` (which is historical). Synthesize in-progress rows at the TOP
of the timeline:

- **Pending review** (`ReviewerApprovalsMissing` / `GateReviewersMissing`
  `{ missing }`): for each missing reviewer of the latest reviewable
  commit, a row in the SAME shape as a finished review but with an
  animated spinner in the verdict-mark field (where ✓/✓✓/✗ would sit,
  `MARK_FIELD` width) + the agent name + a dim "reviewing…". Reviewers
  who HAVE reviewed keep their ✓/✗.
- **Master working** (`MasterToContinue` / `MasterToRevise` /
  `MasterToFinalize`): a synthetic row above the timeline for the commit
  master is producing — `-----` in the sha column (dim) and italic
  "working…" (ANSI `\x1b[3m`), with the spinner for liveness.
- These are TUI-only and ephemeral; do not add them to `OnelineRow` /
  `clank log` (static, no animation).

### Animation tick (architecture — the load-bearing invariant)

A spinner is clock-relative, so it needs a periodic repaint. Periodic
repaint itself is fine and expected — Claude Code and essentially every
TUI animate this way; the cost of redrawing cached text is negligible.

The NON-NEGOTIABLE invariant is what the tick is allowed to TOUCH: an
animation frame MUST be a PURE REPAINT — advance the spinner frame
counter, re-render from the ALREADY-CACHED snapshot, and write to the
terminal. NOTHING ELSE. A tick MUST NOT:
- rebuild the snapshot (`build_async`),
- re-fetch the log (`tui_log_rows` / any git),
- query zellij (`list-panes` / tab/pane name updates),
- read the filesystem or the watcher.

Only real data-change events (the watcher `Refresh`) and explicit user
keys may do IO. Structure the loop so the tick is its OWN branch that
calls only `render_at` (pure) + `paint` — it must NOT fall through the
fill-loop (`tui_log_rows`) or the Refresh path. Make this hard to violate
by construction (a dedicated tick handler that has no access to the repo
path / build fns in its body), and assert in review that the tick branch
contains no IO call. The frame counter is the only state it mutates.

A tick only fires while a spinner/working row is actually on screen
(otherwise there's nothing to animate): when the current frame has an
in-progress indicator, `recv_timeout` with a short interval (~120ms) and
treat a timeout as a tick; otherwise use the plain event-driven backstop.
The frame counter feeds a pure spinner-glyph helper (unit-testable
headless).

## Design (frontend-design skill)

Restraint for a small monospace pane: ONE moving element, monochrome/dim,
instantly legible. A compact spinner cycle (e.g. braille `⠋⠙⠹⠸⠼⠴⠦⠧` or a
4-frame `|/-\\`); pick one and keep it the only motion on screen. Italic
only for "working…". The spinner sits exactly in the verdict-mark column
so a pending review aligns with finished ones — the eye reads a column of
results, some done (✓/✗), some spinning. The master row's `-----` mirrors
a real sha's width so it lines up under the gauges.

## Testing (no-binary-spawning)

- `oneline_rows`: reviews now precede their commit (update existing
  tests + add an explicit order assertion).
- Pure spinner-glyph helper: frame N → expected glyph, cycles.
- Pure builder for the in-progress rows from a `WaitingOn` (given a
  waiting state, the right spinner/working rows are produced) — render
  via the existing span/emit path. The git-backed loop + the actual tick
  timing stay untested, like other TUI glue.

## Acceptance

- `clank log` (oneline) and `status --tui` show each review above its
  commit.
- In `status --tui`: a pending reviewer shows a spinning indicator + name
  in the verdict column; finished reviewers show ✓/✗; master's turn shows
  the `----- working…` row.
- Spinner animates only while work is in progress.
- The animation tick is a PURE repaint: it advances the frame and redraws
  cached state, and triggers ZERO disk/git/zellij/snapshot work (the
  load-bearing invariant — verified structurally, not just by eye). Only
  watcher events and keys do IO.
- cli + core tests green; clippy within budget (cli ≤30); fmt clean.
