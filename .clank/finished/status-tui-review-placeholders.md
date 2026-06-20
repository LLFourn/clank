# status-tui-review-placeholders

Refine the `status --tui` in-progress indicators (from
`status-timeline-progress`) so an animated row is a PLACEHOLDER sitting in
the exact slot its finished row will occupy — under the plan, not
floating above the whole timeline.

## Problem

`status-timeline-progress` (M2) prepends the in-progress rows to the TOP
of the combined timeline — ABOVE the plan's umbrella header — so they
float, visually disconnected from the plan and the commit they relate to.
What we actually want: the animated row is a placeholder for the work in
flight, positioned exactly where the real row will appear, so when the
work lands the spinner is simply REPLACED IN PLACE by the result (no
positional jump).

## Invariant (the core rule)

**Whenever the gate is WAITING ON AN AGENT to produce something, the
timeline shows a spinner in the EXACT slot that output will occupy, with
ITALIC text saying what we're waiting on them for.** When the work lands,
the spinner row is REPLACED IN PLACE by the real row — no positional
jump. One rule for every waiting state; the two cases below are instances
of it. Row shape: `<spinner> <agent-name(dim)> <wait-verb(italic)…>`,
positioned so it becomes the real row. (The wait-verb — not the name — is
what's italic: it's "what we're waiting on them for.")

## Design (instances of the invariant)

- **Waiting on a reviewer → review placeholder.** For the latest
  reviewable commit, render its review block (above the commit, UNDER the
  plan header) with one row PER REGISTERED reviewer: finished reviewers
  show their `✓ / ✓✓ / ✗` row; a reviewer who still owes a verdict shows
  a spinner in the SAME verdict-mark column + the name (dim) + italic
  "reviewing…". When they finish it becomes their verdict row in the same
  slot. The block always shows the full reviewer picture (done +
  in-flight).
- **Waiting on master → next-commit placeholder.** When master is
  producing the next commit, render a placeholder at the top of the
  active plan's section (under the header, above the latest commit) for
  the commit it will become: a spinner + `-------` where the sha will be
  + `snap.master` name (dim, fallback "master") + an italic wait-verb.
  The verb says what we await, per state:
  - `MasterToContinue` → "working…"
  - `MasterToRevise` → "revising…" (addressing review feedback)
  - `MasterToCommit` → "committing…"
  - `MasterToFinalize` → "finalizing…" (making the finish commit) —
    the shipped M2 produced NO row here; this closes that gap.
  (Excluded: `Blocked` → a human's turn, shown by the block ask, no
  spinner; `MasterToFixCommitTag` → already surfaced by the `fix` gauge.)
- **Italic wait-verb in BOTH cases** — review "reviewing…" becomes italic
  too (M2 had it dim), so the invariant reads uniformly.
- **Everything lives UNDER the plan umbrella header**, never above it.
  Remove M2's prepend-to-whole-timeline placement.

## Implementation notes

- `status_tui.rs`: today `render_at` builds `timeline = [in_progress ++
  log_rows]` (in-progress at indices `0..in_prog.len()`, above the
  `OnelineRow::Header` at `log_rows[0]`). Instead, INJECT the in-progress
  rows into the log sequence at the right index:
  - master-working → immediately AFTER the active plan's umbrella
    `Header` (top of that section, above the latest commit);
  - pending-review placeholders → into the latest reviewable commit's
    review block (the rows between the header and the first `Commit`),
    alongside its finished reviews.
- The latest reviewable commit is the first `Commit` row of the active
  plan's section (newest), matching `snap.plans[..].sha`.
- The pending reviewers come from `WaitingOn::ReviewerApprovalsMissing` /
  `GateReviewersMissing { missing }` (as today); master from
  `snap.master`.
- **Animation tick (carry the invariant forward):** the
  viewport-conditional tick currently assumes in-progress rows are a
  CONTIGUOUS block (`offset < in_prog`, or the ask-band intersection).
  After this change they sit at SCATTERED, NON-CONTIGUOUS indices (master
  after the header; reviews inside the latest commit's review block), so
  the check must track the SET of injected indices and fire iff ANY of
  them falls within `[offset, offset + capacity)` — and correctly
  SUPPRESS the tick when all are scrolled off despite being
  non-contiguous. The tick stays a PURE repaint (frame advance +
  render_at + paint, zero IO) — verified structurally.
- Keep it TUI-only and ephemeral; do not touch `OnelineRow` / `clank
  log` (static output has no pending state).

## Design polish (frontend-design)

One moving element, dim/monochrome; the spinner stays in the verdict-mark
column so a placeholder aligns with the finished `✓/✗` it will become —
which is exactly what makes the in-place replacement read as continuous.

## Testing (no-binary-spawning)

Pure rendering/placement tests:
- a pending reviewer renders as a placeholder WITHIN the latest commit's
  review block, under the plan header (not above it), spinner in the
  verdict column, with the wait-verb ITALIC;
- a finished + a pending reviewer on the same commit both appear in that
  block (full reviewer picture);
- master placeholder renders under the header, above the latest commit,
  with `snap.master` + the per-state italic verb, for EACH producing
  state;
- **`MasterToFinalize`: the existing test that pins NO row must FLIP
  (reproduce-first) to assert a "finalizing…" placeholder** — close the
  gap explicitly, not silently;
- tick visibility with SCATTERED indices: an offset that scrolls every
  injected in-progress row out of `[offset, offset+capacity)` suppresses
  the tick (and one that leaves any visible keeps it).

## Acceptance

- **The invariant holds**: for EVERY "waiting on an agent" state there is
  a spinner in the slot that agent's output will occupy, with an italic
  wait-verb saying what we await — and when the work lands the spinner is
  replaced in place (no positional jump).
- A pending review shows as a spinner in the slot its `✓/✗` will occupy,
  above the commit, under the plan header, with italic "reviewing…".
- The master placeholder shows `<master>` + the per-state italic verb
  (working… / revising… / committing… / finalizing…), under the plan
  header — including the previously-missing `MasterToFinalize` case.
- No in-progress row floats above the plan umbrella header.
- The animation tick still fires only when an in-progress row is actually
  on screen (scattered-index visibility), and remains a pure (IO-free)
  repaint.
- Existing status TUI tests green (incl. the flipped `MasterToFinalize`
  test); clippy within budget (cli ≤30); fmt clean.
