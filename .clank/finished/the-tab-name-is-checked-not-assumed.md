# the-tab-name-is-checked-not-assumed
# The zellij tab name is checked against what it should be, not assumed from the last rename

## Why

In `~/src/anxiety-attack` the repo has been idle since `consistent-draft-font` finished on
8 Oct, and the master pane reads `💤 codex (master)`. But its zellij tab (in session
`clank-life`) still reads `🔨 anxiety-attack`. Only the repo's status TUI renames that tab
(`status_tui::zellij::TabIndicator`), and that TUI is running (`clank tui --repo
~/src/anxiety-attack`, started 6 Oct).

`TabIndicator::update` renames only when the glyph differs from `last`, and it sets `last`
whether or not the rename happened. `open_zellij::rename_tab` discards zellij's result
(`let _ = …output()`). A rename that fails (zellij busy, an action that errors, a tab id
that moved) is therefore recorded as done, and the tab stays stale until the glyph
changes again, which for an idle repo may be never. `last` is a belief about the tab, not
a reading of it.

Confirmed live after this plan was drafted: the anxiety-attack TUI's own bar reads `💤 idle`
(`zellij action dump-screen` of its pane) while its tab reads `🔨 anxiety-attack`. The TUI's
snapshot is right and the tab is not, so the rename to 💤 did not take. No log says why,
because `rename_tab` discards zellij's output.

Two more defects make a missed rename permanent, not just possible (codex on 081faae):

- **Updates only happen on a snapshot change.** `run_tui` calls `tab.update` only inside
  `sig != last_sig`, after a rebuild. A failed rename, or a tab name edited by something
  else, changes neither the signature nor the glyph, so nothing ever looks again. The first
  successful build does not call it at all.
- **Reading "the current tab" is not reading ours.** `current_tab_info` runs `zellij action
  current-tab-info`, which reports the user's ACTIVE tab, not the tab holding the TUI. That
  holds at startup too: a session can start or resume with another tab active, and focus
  can move before anything is captured. An id captured from the active tab makes the
  indicator rename and restore the wrong tab for the TUI's whole life.

## What to build

The model is the wanted name, plus the owned tab's stable id and base name, once ownership
is proved. The tab's
actual name, read by that id, decides whether to rename. Nothing caches "what I last
asked for" in a way that suppresses looking.

- **The zellij worker owns the tab name, as it already owns pane titles.** The loop sends
  the wanted glyph with the glyph data it already sends (`update_glyphs`), including after
  the first build. The worker keeps the latest wanted name. On EVERY batch it handles,
  including its existing `PRESENCE_PERIOD` probe, it reads the session's tabs with
  `zellij action list-tabs --json`, finds the owned id, and renames by id only when the
  actual name differs. The probe is the existing cadence, so this adds no loop, timer,
  or zellij call on the UI or animation thread.
- **Ownership is proved from the TUI's own pane, never from focus.** The worker resolves
  the TUI's pane (`open_zellij::caller_pane_id`, from `ZELLIJ_PANE_ID`) against the pane
  listing it already takes for reconciliation, and adopts that pane's `tab_id`. The base
  name comes from the tab listing, by that id, with any leading status glyph stripped as
  today. Until ownership is proved, the wanted glyph is kept, and nothing is renamed or
  restored. A failed or ambiguous first lookup stays unresolved and is retried on a later
  batch. No pane id in the environment means no indicator. There is no fallback to the
  active tab or to any other pane. Once proved, the id is kept; if that tab disappears, the
  indicator does nothing and never adopts another tab. `current_tab_info` is no longer
  used for this.
- **Observation failures are unknown, not success.** A failed or malformed listing, or a
  listing without the owned id, means no rename and no bookkeeping. The next batch looks
  again. Focus moving to another tab changes nothing: the lookup is by the owned id,
  never the active tab, never a position, and the id is never replaced.
- **A rename reports whether zellij accepted it.** `rename_tab` returns the command's
  success. The only thing success records is that the tab was ever renamed, which is what
  the restore-on-exit contract needs. Convergence comes from the next read, not from a
  record, so a rename that was accepted but did not apply is also corrected.
- **Restore on exit, after pending work.** When the worker stops, after its final batch,
  it renames the owned tab back to its base if it ever renamed it successfully. A rename
  that only ever failed restores nothing, and no pending batch can rename after the restore.
- **Audit, reported:** `PaneStatus::update_with` has the same shape: it records `last`
  after a void rename, and `rename_pane` discards the status. Report it in As built; fixing
  pane titles is not this plan.

## Acceptance

With the zellij reads and renames injected:
- A rename that fails, followed by batches with the same wanted name, retries until the tab
  reads right. Against today's `update`, which records before knowing, it would not.
- A tab renamed by something else is put back on the next batch, with no new glyph.
- A correct name causes no rename.
- A failed listing, a malformed one, and one missing the owned id each cause no rename, and
  the next good batch converges.
- Ownership: with the TUI's pane in tab A and tab B active at the very first observation,
  only A is ever renamed or restored, and B is never touched. The same holds when focus
  moves before the worker's first batch. An ownership lookup that fails, then succeeds,
  renames nothing until it succeeds, then converges A. A missing pane id renames nothing.
- After ownership, another tab being active changes nothing: only the owned id is read and
  renamed.
- Restore after a successful rename; no restore when every rename failed; no rename after
  the restore.
- Through the worker: an unchanged snapshot (only probes arriving) still converges a stale
  tab, and the first build's glyph reaches the tab.
- Mutation-checked: recording before success, skipping the actual-name comparison,
  reconciling only on new glyphs, reading the active tab instead of the owned id, and
  taking ownership from the active tab instead of the TUI's pane each fail a test.

## Out of scope

What glyph the bar shows (`bar_emoji`), the TUI's refresh triggers, and pane-title
reconciliation.

## As built

- `TabIndicator` is gone, with `current_tab`, `parse_current_tab_info` and
  `open_zellij::current_tab_info`. The run loop no longer touches the tab. It sends the
  bar's glyph with the glyph data (`StatusGlyphs.tab`), and now also after the first build,
  which before reached neither the tab nor the pane titles until the first refresh.
- `status_tui::zellij::TabMirror`, owned by `WorkerState`, holds the TUI's pane
  (`caller_pane_id`), the proved tab (id and base name), the wanted glyph, and whether a
  rename was ever accepted. `handle_with` reconciles it on every batch that lists,
  probes included, through `TabIo` (`list_tabs`, a `rename_tab` that now returns whether
  zellij accepted). Ownership comes from `open_zellij::owned_tab`: exactly one terminal
  pane with the caller's id in the batch's pane listing, in a tab the listing names.
  `ZellijPane.tab_id` is now `Option<u32>`, so a missing tab is unknown, not tab 0;
  existing readers go through `tab()`. `parse_tabs` rejects a tab without an id.
- `serve` is the worker thread's body: every batch, then `TabMirror::restore`, so nothing
  queued can rename after the base name is back.
- Audit: `PaneStatus::update_with` records a pane's title after a void rename, and
  `open_zellij::rename_pane` discards zellij's status. Pane titles therefore have the same
  assumed-success shape. Left for its own change, as scoped.
- Mutation-checked, each caught: recording before success, skipping the actual-name
  comparison, reconciling only on new glyphs, re-owning every batch, restoring before the
  batches, taking ownership from the focused pane, and reading a missing tab as tab 0.
