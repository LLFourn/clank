# adhoc-reviews-in-log
# ad-hoc reviews render in the log surfaces

In ~/src/fsctl (review.adhoc_feedback on), commit 0738e46 carries a
codex REQUEST_CHANGES in its feedback file, but `clank status --tui`'s
log, `clank log --oneline`, and `clank log` all render the commit bare
(lloyd, 2026-07-13). The DATA pipeline is fine — reviewable_shas
includes `LogEvent::AdHoc` in every caller, collect_reviews fetches
them, and the JSON renderer already emits them. Two RENDER sites
suppress them, both encoding the pre-adhoc_feedback model "AdHoc rows
carry no reviews":

- `oneline_rows` (log.rs ~445) — the explicit
  `!matches!(event, LogEvent::AdHoc { .. })` gate; this one site
  blanks BOTH `clank log --oneline` and the TUI (which builds its rows
  through the same function).
- `print_human`'s AdHoc arm (log.rs ~225) — prints the commit and
  `continue`s before the review-rendering path.

## Fix

- Drop the AdHoc gate in `oneline_rows`: reviews render above their
  commit exactly like plan commits (same newest-first, above-the-
  commit placement, deterministic by-author order); fix the stale
  comment.
- `print_human`'s AdHoc arm renders the commit's reviews the same way
  the plan-commit path does (whatever shape that path uses — one
  shared helper if extraction is trivial, no big refactor).
- The JSON path stays as-is (already correct) but gains a pinning
  test so the three surfaces can't diverge again.

## Acceptance

- Fixture: an AdHoc event with a review → `oneline_rows` emits the
  Review row above the commit (this one test covers --oneline AND the
  TUI); `print_human`-level coverage for the same; a JSON test pins
  the already-working behavior.
- Plan-commit rendering byte-identical (existing tests untouched).
- fmt/clippy at the 18/6 baseline; suites green.
