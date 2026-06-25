# status-commit-detail

## Problem

In `clank status --tui` you can scroll the log/timeline, but the rows
are one-liners: a commit shows only its subject, a review only a summary
line. There's no way to read a commit's full message or a reviewer's
full feedback without leaving the TUI.

## Goal

In `LogScroll`, pressing Enter on the selected log entry opens a
**full-window** detail view for that commit: its full message plus every
reviewer's full feedback. Esc / q / ← / Enter returns to the log.

## Design

Mirror the existing full-screen detail pattern (`render_agent_detail` /
the `AgentDetail` mode) — this builds on the `status-tui-modules`
refactor, so the pieces land in the new modules.

### Mode + identity

- Add a mode variant `CommitDetail { sha: CommitSha, offset: usize }`
  (in the input module). Anchor to the **commit sha**, not a log index:
  the log refetches/refreshes and resizes, so an index would drift to a
  different entry (same class of bug as the console selection that was
  bound to a slot instead of a concrete screen). `offset` is the scroll
  position within the detail.
- The sha a log entry maps to:
  - A `Commit` row → its own sha.
  - A `Review` row → the commit it reviews (reviews are grouped with
    their commit in the scroll model; resolve to that commit's sha).
  - `Header` / in-progress placeholder rows → Enter is a no-op.
  Add a small pure helper `entry_commit_sha(seq, cursor) -> Option<CommitSha>`
  so this mapping is unit-tested.

### Data (all already in the layer)

Fetched in the loop (IO shell) on entry and re-fetched on `Refresh`
while the view is open, then passed as plain data into a pure renderer:

- Full commit message: `git_io::commit_body_at(repo, &sha)` (+
  `commit_subject_at` for the heading).
- Per-reviewer feedback: `crate::feedback_scan::scan_feedback(repo,
  &[sha])` → `clank_core::feedback_body::FeedbackBody` for each
  reviewer's verdict + full body (same source the log's `Review`
  summaries already come from).

### Render

- `render_commit_detail(detail_data, offset, rows, cols) -> Vec<String>`
  in the render module: full window (returns `(lines, 0)` like
  `render_add_screen`), a region rule heading (`commit <short-sha>` +
  back hint), the subject, the wrapped body, then a section per reviewer
  (`<verdict glyph> <author>` followed by the wrapped feedback body),
  reusing the existing `wrap` / span / `region_rule` primitives. Scrolled
  by `offset`, clamped to content height.
- The main render dispatches to it when `mode == CommitDetail`, replacing
  the normal layout (as `AddPicker` / `AgentDetail` already do).

### Input

- `LogScroll` + Enter on a resolvable entry → `CommitDetail { sha,
  offset: 0 }`.
- In `CommitDetail`: Up/Down/PageUp/PageDown/Space scroll (clamped);
  Esc / q / ← / Enter → back to `LogScroll` (cursor preserved). Routed
  by a pure function mirroring `agent_detail_nav`.

## Acceptance

- Enter on a commit or review row opens the detail for the correct
  commit; Header / in-progress rows do nothing.
- Detail shows the full commit body and each reviewer's full feedback,
  scrollable, taking the whole window; back-keys restore the log with
  the cursor where it was.
- Pure tests: `entry_commit_sha` mapping (commit row, review row,
  header/in-progress → None); `render_commit_detail` output contains the
  body + each reviewer's verdict/body and is full-window; nav routing
  (scroll clamps, back-keys exit).
- `cargo test -p clank --lib` green; fmt + clippy clean.

## Depends on

`status-tui-modules` (lands in the refactored module layout). Promote
after that plan finishes.

## Out of scope

- Editing feedback or any action from the detail view (read-only).
- A diff/patch view of the commit (subject + body + feedback only;
  `git_io::commit_show_patch` could power a later extension).
