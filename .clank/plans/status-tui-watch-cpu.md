# status-tui-watch-cpu

`clank status --tui` panes collectively burn ~44%+ CPU (observed
2026-06-18: two zellij servers at 51% + 31%, FSEvents at 36.6%, 12
status-tui panes across worktrees). Two independent causes, both in
`status_tui.rs` / `status.rs`.

## Cause 1 — the fs-watch monitors the WHOLE repo (incl. `target/`)

`watch_status_paths` (status.rs) does
`watcher.watch(repo, RecursiveMode::Recursive)` — it watches the
entire repository tree, then uses `WakeFilter` to decide whether each
event should wake a re-render.

The gitignore subtlety (lloyd's question): `notify`/FSEvents watches a
path **regardless of `.gitignore`**. The gitignore matcher in
`WakeFilter` only suppresses the *wake*, NOT the OS-level watch. So
`target/` (which churns massively during `cargo build`) is fully
monitored by FSEvents — 12 panes × whole repo trees including
`target/` and `.git/objects/`. That's the FSEvents 36.6%, paid even
though `WakeFilter` never wakes on `target/`.

Why "wake only on tracked files" is NOT the fix: clank's own state
under `.clank/` (`agents/<>/feedback`, `blocks`, `cache`, `queue`) is
GITIGNORED but is exactly what the loop must react to (a reviewer
feedback write flips the gate). A tracked-only wake filter would go
deaf to clank's own signals. The wake criterion is already right; the
WATCH SCOPE is wrong.

### Fix 1

Watch only the dirs we actually care about — `.clank/` + the git dir —
NOT the whole repo. This is exactly what `wfw`'s `WatchContext`
already does (`wfw.rs`); `status --tui` should match it. FSEvents then
never monitors `target/` at all.

- Trade-off: `dirty: +X −Y` no longer refreshes on every source-file
  save (we stop watching `src/`); it refreshes on the heartbeat and on
  `.clank/`/git events instead. Fine for a passive monitor pane — and
  the heartbeat could be shortened (e.g. 60s → a few seconds) if live
  dirty matters, still far cheaper than watching the whole tree.
- Verify no `.clank/cache/` self-trigger: if the tui's re-fold writes
  under `.clank/cache/` and we wake on all of `.clank/`, that's a
  render→write→wake loop. Confirm the tui uses a read-only cache
  policy, or exclude `.clank/cache/` from waking.
- `wfw`'s `WatchContext` is already narrow — this cause is
  status-tui-specific.

## Cause 2 — `PaneStatus` shells `zellij action list-panes` every render

`PaneStatus::update` (status_tui.rs, from tui-agent-pane-status-emoji)
calls `zellij_list_panes()` UNCONDITIONALLY at the top of every
`update()` — i.e. every render/wake — before any dedup. Each call is a
`zellij action list-panes` subprocess + a round-trip to the zellij
SERVER. During active clank work the loop wakes up to ~5×/s (200ms
debounce), so each new-binary pane hammers its zellij server several
times a second. This is the regression behind the 51%/31% server CPU
(newer `clank-clank` ran ~10%/pane vs older sparrow ~5.7%/pane).

### Fix 2

The pane-id map (`label (role)` → pane id) is session-stable, so:
- Fetch `list-panes` ONCE (lazily on first update), cache the map.
- On later updates, compute each agent's emoji (pure) and rename ONLY
  when it changed (the existing `self.last` dedup) — using the cached
  id. No `list-panes` per render.
- Re-query `list-panes` only when a desired rename target isn't in the
  cache (a pane was added). `TabIndicator` is already efficient
  (captures the tab id once, renames only on emoji change) — leave it.

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- Pure: the watch-target set is `{.clank, git_dir}` not the repo root
  (assert on the resolved watch paths, or a small seam returning them).
- Pure: `PaneStatus` calls `list-panes` once then reuses the cache —
  inject a counting fake for the list-panes call; assert it's not
  invoked per `update`, and that a rename fires only on emoji change.
- The actual FSEvents/zellij CPU is verified manually (close panes /
  observe `ps`); not unit-testable.

## Non-goals

- `wfw`'s wake reliability ([[wfw-indefinite-wait-resilient]]) —
  separate plan; this one is purely the status-tui watch + pane-rename
  cost.
- Changing the emoji vocab / which states show which glyph.
- Reimplementing dirty-stat computation (only its refresh cadence
  changes).
