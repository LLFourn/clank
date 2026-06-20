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

## Design

- **Pending review = a review placeholder.** For the latest reviewable
  commit, show its review block (above the commit, UNDER the plan header)
  with one row PER REGISTERED REVIEWER: a finished reviewer shows their
  `✓ / ✓✓ / ✗` row (as today); a reviewer who still owes a verdict shows
  a spinner in the SAME verdict-mark column + "<name> reviewing…". When
  that reviewer finishes, the placeholder becomes their verdict row in
  the same slot — continuous, no jump. Net effect: the latest commit's
  review block always shows the full reviewer picture (done + in-flight).
- **Master working = a next-commit placeholder.** When master is
  producing the next commit, show a placeholder at the TOP of the active
  plan's section — under the umbrella header, above the latest commit —
  labeled "<master> working…" using `snap.master` (fallback "master"),
  with the spinner for liveness. This covers ALL master-producing states:
  - `MasterToContinue` / `MasterToRevise` / `MasterToCommit` (impl /
    revision in flight), AND
  - **`MasterToFinalize`** — finalizing IS making the finish commit, so
    it gets a spinner placeholder too (the shipped M2 wrongly excluded
    it, so the gate-FINISHED → master-finalizing moment shows nothing).
    The row may read "<master> finalizing…" to distinguish the finish
    commit, or stay "<master> working…"; either way it animates.
  (Still excluded: `Blocked` — that's the block ask, a human's turn, no
  spinner — and `MasterToFixCommitTag`, an amend already surfaced by the
  `fix` gauge.)
- **Both live UNDER the plan umbrella header**, never above it. Remove
  M2's prepend-to-whole-timeline placement.

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
  viewport-conditional tick currently assumes in-progress rows are at
  offset 0 (`offset < in_prog_count`). They now sit at computed indices,
  so the tick must fire iff ANY in-progress row index falls within the
  visible window `[offset, offset + capacity)`. The tick stays a PURE
  repaint (frame advance + render_at + paint, zero IO) — unchanged and
  still verified structurally.
- Keep it TUI-only and ephemeral; do not touch `OnelineRow` / `clank
  log` (static output has no pending state).

## Design polish (frontend-design)

One moving element, dim/monochrome; the spinner stays in the verdict-mark
column so a placeholder aligns with the finished `✓/✗` it will become —
which is exactly what makes the in-place replacement read as continuous.

## Testing (no-binary-spawning)

Pure rendering/placement tests:
- a pending reviewer renders as a placeholder WITHIN the latest commit's
  review block, under the plan header (not above it), in the verdict
  column;
- a finished + a pending reviewer on the same commit both appear in that
  block (full reviewer picture);
- master-working renders "<master> working…" under the header, above the
  latest commit, using `snap.master` — for EACH producing state, incl.
  `MasterToFinalize` (the finish-commit case the shipped code missed);
- tick visibility: given an offset that scrolls the in-progress rows out
  of `[offset, offset+capacity)`, the tick is suppressed.

## Acceptance

- A pending review shows as a spinner in the slot its `✓/✗` will occupy —
  above the commit, under the plan header — and is replaced in place when
  the review lands (no positional jump).
- The master-working row reads "<master> working…" (the master's label),
  under the plan header.
- No in-progress row floats above the plan umbrella header.
- The animation tick still fires only when an in-progress row is actually
  on screen, and remains a pure (IO-free) repaint.
- Existing status TUI tests green; clippy within budget (cli ≤30); fmt
  clean.
