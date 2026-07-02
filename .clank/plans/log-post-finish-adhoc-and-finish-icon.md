# log-post-finish-adhoc-and-finish-icon

Timeline-rendering fixes in `clank log --oneline` AND `status --tui` (both
consume the same shared row producer):

1. An ad-hoc commit that lands AFTER a plan's `finish` commit is wrongly
   folded UNDER that (finished) plan's umbrella. It should be its own
   (header-less) ad-hoc section — the plan is done.
2. The finish commit renders as a synthesized `finish` with no visual marker.
   Now that finish messages are real whole-plan summaries (not "finish"),
   surface the REAL message and mark the finish commit.
3. (Extended per lloyd) A full commit-row ICON TAXONOMY, icon LEADING each row
   (before the sha), disambiguating every commit at a glance: finish `⚑`,
   implementation `⚒` (touched code), planning `✎` (intro / plan-doc-only), and
   ad-hoc `~` (moved to the front). Chosen as 1-COLUMN symbols (not emoji) to
   preserve the tight gutter and alignment (lloyd's pick).

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

## Fix B/C — real finish message + full commit-row icon taxonomy

The finish message is now meaningful, but `oneline_rows` synthesized
`format!("[{}] finish", plan)` because `LogEvent::PlanFinalized` carried no
subject. And every commit should lead with an icon identifying its kind.

- **Carry the subject** (`crates/core/src/repo_state.rs`): `subject: String`
  added to `LogEvent::PlanFinalized`, populated from `event.subject` at the
  fold emit site. `#[serde(default)]` for graceful degrade PLUS a
  `CACHE_FORMAT_VERSION` bump (11→12) — RESOLVED: `serde(default)` alone is a
  trap (pre-subject checkpoints deserialize blank and aren't re-folded on
  incremental appends, so existing finishes render blank); the version bump
  forces a one-time re-fold that populates them (ruthless ab4e174). `--json`
  Finalized row carries `subject` too.
- **Render the real subject** (`oneline_rows`): use the carried subject with
  the same `[plan]`-prefix stripping as other rows; fall back to the
  synthesized `[plan] finish` only when subject is empty.
- **`RowMarker` taxonomy** replaces `ad_hoc: bool` on `OnelineRow::Commit`:
  `{ Plain, AdHoc, Planning, Impl, Finish }` (mutually exclusive). `RowMarker::of`
  classifies: AdHoc→`~`, PlanFinalized→Finish `⚑`, PlanIntro→Planning `✎`,
  PlanCommit→Impl `⚒` if `touched_code` else Planning `✎`, PlanDeleted→Plain.
  The icon LEADS the row (before the sha) — updated in all three renderers:
  `oneline_plain_lines` (TUI plain), `print_oneline` (CLI colored, per-marker
  color), and `log_row_spans` (TUI span model).

## Decisions — RESOLVED

1. **Glyphs / gutter width.** 1-COLUMN symbols (lloyd's pick): `⚑` finish,
   `⚒` impl, `✎` planning, `~` ad-hoc. Preserves the tight gutter + alignment.
   Emoji (🔨/📜/🏁) are 2-col and would break it — rejected. Width pinned by a
   test against the pane's own `char_width` (finish/impl/plan/adhoc all == 1).
2. **Marker shape.** `RowMarker` enum (not `finish: bool`) — extended to the
   full taxonomy.
3. **Cache.** `CACHE_FORMAT_VERSION` bump (not `serde(default)` alone) so the
   real finish message applies retroactively.

## Tests

- `umbrella_sections` (core): the reported post-finish-adhoc case → the adhoc
  is its OWN section, not under the finished plan; an adhoc BEFORE the finish
  (active plan) still folds in; existing fold tests still pass.
- `oneline_rows` (cli): finish row carries the real subject (prefix-stripped);
  markers classify correctly (code commit → Impl, drive-by → AdHoc); the
  icon-led layout aligns subjects across markers (char position, since glyphs
  are multi-byte); each glyph is one char.
- `char_width` invariant (status_tui): every `RowMarker` glyph is 1 display
  column per the pane's own width model.

## Acceptance criteria

- Post-finish ad-hoc commits render OUTSIDE the finished plan's umbrella in
  `clank log --oneline` and `status --tui`.
- Each commit row LEADS with its kind icon (⚑/⚒/✎/~); the finish commit shows
  its real whole-plan message; alignment preserved (all 1-col).
- Existing umbrella/oneline/tui tests green; clippy at baseline.

## Deploy

`cargo install --path crates/cli --force`.
