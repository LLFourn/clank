# zellij-focus-restore

`clank agent add <name>` run from a zellij tab OTHER than the one holding
the repo's agent panes leaves you stranded on the main worktree tab
instead of returning you to the tab you ran the command from. The roster
mutation is correct; only the focus restore is wrong.

## Root cause (the model, not the symptom)

`add_reviewer_pane` (open_zellij.rs) stacks the new reviewer by focusing
an anchor pane in the agents' tab, creating the pane, then restoring
focus to "where the caller was". It computes that restore target with
`focused_pane_id(&panes)` = `panes.iter().find(|p| p.is_focused)`.

But `zellij action list-panes --json` returns panes across ALL tabs, and
each tab reports its own active pane as `is_focused: true`. In a
multi-tab session there are SEVERAL focused panes; `.find()` returns the
first in JSON order — the agents'/worktree-root tab's active pane — not
the pane the command actually ran in. So the "restore" focuses the wrong
tab. It only works in a single-tab session (one `is_focused` pane).

The deeper issue: the caller's pane identity is being INFERRED from an
ambiguous global focus scan, when zellij hands it to us exactly via the
`ZELLIJ_PANE_ID` env var. Reading-and-restoring ambient global focus is
the smell; the caller already knows its own pane.

## Fix

- Add a `caller_pane_id() -> Option<String>` that reads `ZELLIJ_PANE_ID`
  and forms the pane ref `terminal_<id>` (a clank command always runs in
  a terminal pane, never a plugin). Keep it env-injectable for testing:
  the pure part takes the raw id and returns the ref; the wrapper reads
  the env.
- In `add_reviewer_pane`, derive the restore target from
  `caller_pane_id()`, falling back to `focused_pane_id(&panes)` only if
  `ZELLIJ_PANE_ID` is unset (defensive; it's always set inside zellij).
- Keep everything best-effort / `$ZELLIJ`-gated / `.output()`-isolated:
  the config change is the source of truth and persists regardless of any
  focus outcome.

## Similar commands — audit, don't assume

`focused_pane_id` has exactly one live caller today (`add_reviewer_pane`).
But check every zellij focus/pane op for the same "strands you on the
wrong tab" class:

- `remove_reviewer_pane` (`close-pane --pane-id`): closing a pane in
  another tab, or the caller's own pane. Confirm it does not move the
  caller's focus; if it can, route its restore through `caller_pane_id()`
  too.
- This primitive is what the queued `agent-promote-zellij-relocation`
  plan will need for its two coupled moves — landing it first de-risks
  that work (it must also return focus to the caller, not strand it).

## Testing (no-binary-spawning)

- Pure: a multi-tab `list-panes --json` fixture with TWO `is_focused`
  panes (one per tab). Assert the restore target resolves to the caller's
  pane (the one matching the injected `ZELLIJ_PANE_ID`), NOT the first
  `is_focused`. Drive it through the pure helper so no env mutation /
  process-global race.
- Keep the existing `focused_pane_id_finds_the_focused_pane` test as the
  documented fallback behavior.
- The live `focus-pane-id` / `new-pane` calls stay untested like the rest
  of the zellij glue.

## Acceptance

- Running `clank agent add` (and `agent remove`) from ANY tab leaves
  focus on the pane you ran it from — no persistent tab switch — while the
  new/closed pane still lands in the agents' stack.
- No-op outside zellij; a zellij hiccup never fails the roster mutation.
