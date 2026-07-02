# log-post-finish-adhoc-and-finish-icon

Two related timeline-rendering fixes in `clank log --oneline` AND `status
--tui` (both consume the same shared row producer):

1. An ad-hoc commit that lands AFTER a plan's `finish` commit is wrongly
   folded UNDER that (finished) plan's umbrella. It should be its own
   (header-less) ad-hoc section — the plan is done.
2. The finish commit renders as a synthesized `finish` with no visual marker.
   Now that finish messages are real whole-plan summaries (not "finish"),
   surface the REAL message and mark the finish commit with a flag glyph so
   it's identifiable at a glance.

Reproduced in `frostsnap/.clank/worktrees/full-app-sim-driver`: `clank log
--oneline` shows an ad-hoc `~ rename the app-sim CLI…` commit under the
`sim-recovery-test` umbrella, above that plan's `finish` commit — but it
landed after the finish.

## Fix A — post-finish ad-hoc is not under the finished plan

`crates/core/src/repo_state.rs::umbrella_sections` (the shared grouping used
by log/tui/html). In the `newest_first` branch, pending ad-hocs are flushed
into the NEXT plan run. The finish commit (`LogEvent::PlanFinalized`) is a
plan's chronologically-newest event, so in newest-first order any ad-hoc
appearing BEFORE it in the stream is NEWER than the finish → it landed after
the plan finished. So: when the plan event about to absorb pending ad-hocs is
`PlanFinalized`, flush the pending ad-hocs as their OWN `AdHoc` section
instead of folding them into that plan's run.

```rust
let is_finish = matches!(e, LogEvent::PlanFinalized { .. });
if is_finish && !pending.is_empty() {
    out.push((UmbrellaKey::AdHoc, std::mem::take(&mut pending)));
}
// …then the existing new-run / append(pending) / push(e) logic (pending now
// empty for the finish case).
```

This only special-cases finish: an ad-hoc between an ACTIVE plan's commits
(absorbing event is intro/revise, not finish) still folds in correctly, and
an ad-hoc during a later active plan still folds into THAT plan. Verified by
walking the reported case and the interleave cases.

## Fix B/C — real finish message + flag marker

The finish message is now meaningful, but `oneline_rows`
(`crates/cli/src/cli/log.rs:343`) synthesizes `format!("[{}] finish", plan)`
because `LogEvent::PlanFinalized` carries no subject. So:

- **Carry the subject** (`crates/core/src/repo_state.rs`): add `subject:
  String` to `LogEvent::PlanFinalized`; populate it from `event.subject` at
  the fold emit site (grep `PlanFinalized {`). Cache back-compat: `LogEvent`
  is serde (not wincode) — add `#[serde(default)]` on the new field so cached
  fold checkpoints without it still deserialize (or bump the fold-cache
  version). CONFIRM which the cache path needs.
- **Render the real subject** (`oneline_rows`): use the carried subject for
  `PlanFinalized` (with the same `[plan]`-prefix stripping as other rows via
  `parse_subject`), instead of the synthesized `[plan] finish`.
- **Mark it**: add a finish marker to `OnelineRow::Commit`. Replace
  `ad_hoc: bool` with `marker: RowMarker { Plain, AdHoc, Finish }` (mutually
  exclusive by construction — a finish commit is never ad-hoc), or add a
  `finish: bool` alongside (decision below). Renderers show a flag glyph in
  the existing 1-col gutter for finish rows:
  - `oneline_plain_lines` (the TUI plain path) — gutter char.
  - `print_oneline` (CLI colored path, log.rs ~449) — gutter + color.
  - the status TUI's `OnelineRow` span renderer (grep `tui_log_rows` /
    OnelineRow in `status_tui/`) — add the glyph span there too.

## Decisions to flag for review

1. **The glyph.** Lean `⚑` (U+2691 BLACK FLAG) — a planted flag = completion,
   and crucially it's 1-col so it preserves the fixed 1-col marker gutter that
   keeps every row aligned in the narrow TUI pane. Emoji flags (`🏁` victory /
   checkered, `🚩`) are 2-col (East-Asian Wide) and would BREAK the gutter
   alignment — avoid unless we widen the gutter for all rows. User's call on
   the exact glyph given that constraint.
2. **Marker shape.** `RowMarker` enum (Plain/AdHoc/Finish) vs. an added
   `finish: bool`. Lean enum — the three states are mutually exclusive and an
   enum makes an ad-hoc-AND-finish row unrepresentable.
3. **`PlanFinalized.subject` cache**: `#[serde(default)]` (back-compat, keeps
   old checkpoints) vs. cache-version bump (forces a re-fold). Lean
   `#[serde(default)]`.

## Tests

- `umbrella_sections` (core): the reported case — `[adhoc, finish(A),
  revise(A), intro(A)]` newest-first → sections `[AdHoc:[adhoc],
  A:[finish,revise,intro]]`, NOT the ad-hoc under A. Plus: an ad-hoc BEFORE
  the finish chronologically (`[finish(A), adhoc, revise(A)]`) still folds
  into A (it was active); the existing fold tests still pass.
- `oneline_rows` (cli): a `PlanFinalized` row carries the real subject
  (prefix-stripped) and is marked `Finish`; a post-finish ad-hoc renders as a
  header-less ad-hoc row, not under the finished plan's header.
- Renderer smoke: `oneline_plain_lines` shows the flag glyph in the gutter on
  a finish row (and the TUI span path, however that's unit-testable).

## Acceptance criteria

- Post-finish ad-hoc commits render OUTSIDE the finished plan's umbrella in
  `clank log --oneline` and `status --tui`.
- The finish commit shows its real whole-plan message and a flag glyph marker
  in both surfaces; alignment is preserved (1-col glyph).
- Existing umbrella/oneline tests green; clippy at baseline.

## Deploy

`cargo install --path crates/cli --force`.
