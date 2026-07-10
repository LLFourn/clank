# tui-zellij-pane-reconcile
# tui owns zellij pane reconciliation

Today the zellij agent-pane layout is only adjusted by the two commands
that happen to call `open_zellij::add_reviewer_pane` /
`remove_reviewer_pane` inline (`clank agent add` at agent.rs:699,
`clank agent remove` at agent.rs:979). Every other way the roster can
change — TUI actions, `clank init --team`, a manual `.clank/config.json`
edit — leaves the panes stale. Replace the scattered per-command calls
with ONE reconciler: the status TUI already refreshes its agent list
when the config watcher fires; make that same refresh reconcile the
zellij panes.

## Why

Roster state has one source of truth (the repo config) but pane state
is only updated on two of the many write paths — a classic
sometimes-reconciled split-brain. Putting reconciliation where the
config CHANGE is observed (the TUI's watcher-driven refresh) covers
every path with one mechanism, including edits made while no clank
command ran at all.

## Behavior

- When the TUI runs inside zellij (`$ZELLIJ`), each snapshot refresh
  whose roster SET differs from the previous snapshot (labels, not
  roles — a role flip keeps the same pane) reconciles the current
  repo's agent panes:
  - roster member with no matching agent pane → open one in the
    existing agent stack (the current `add_reviewer_pane` anchoring),
    running `clank agent start <label>` for this repo.
  - agent pane whose label is no longer in the roster → close it
    (`close-pane` kills the pane's process tree, which is what
    guarantees the agent exits).
- Reconcile once at TUI startup too: a change made while the TUI was
  closed must be caught when it opens ("no gaps").
- Focus preservation is a hard requirement: reconciliation runs from
  the status pane on a watcher event, possibly while the user types in
  another pane — it must never leave focus somewhere new. Reuse/extend
  the existing restore machinery so focus ends where it was when the
  reconcile began.
- Everything stays best-effort and idempotent exactly like the
  existing helpers: no-op outside zellij, on query failure, or when
  the pane set already matches. Pane matching stays by exact
  `clank agent start <label> --repo <path>` command, so the reconciler
  only ever touches THIS repo's agent panes.

## Strip-out

- Remove the inline calls from `clank agent add` / `clank agent
  remove` (agent.rs:699, 979). The helpers themselves stay — the TUI
  becomes their only caller. Commands become pure config mutations;
  the running TUI notices and reconciles.
- Sweep for any other command-side pane fiddling that this model
  obsoletes (one grep over `add_reviewer_pane|remove_reviewer_pane|
  close-pane|new-pane` in cli/, keeping `clank fork`'s new-TAB opening
  and the TUI's tab/pane RENAME mirroring — both are different
  concerns and explicitly stay).

## Accepted scope

- No TUI running → no reconciliation. The status TUI is the standing
  dashboard of a clank zellij layout; making it the reconciler is the
  point of this plan.
- Two TUIs on one repo could race; the pane-exists/absent checks make
  duplicate work a no-op, and the loser's actions converge to the same
  target state.

## Acceptance

- With the TUI open in zellij: adding a member to the roster by ANY
  means (TUI, CLI, hand-editing config.json) opens its agent pane;
  removing one closes its pane and the agent process exits.
- A role-only change (commit → gate, or master swap between existing
  members) opens/closes nothing.
- Reconciliation never moves the user's focus.
- `clank agent add`/`remove` no longer touch zellij themselves (their
  tests updated accordingly); outside zellij nothing changes at all.
- Pure logic (roster-vs-pane diffing, reconcile planning) is
  unit-tested without spawning zellij, same style as the existing
  parser tests; no binary-spawning tests.
- fmt/clippy at the 18/6 baseline; tests green.
