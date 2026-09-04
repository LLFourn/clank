# reopen-an-agents-pane-from-its-menu

An agent whose zellij pane is gone — closed by hand, or its tab
reopened without it — stays on the roster with no way back into the
workspace short of `clank agent start <label>` in a pane the user
splits themselves. The status TUI's per-agent menu already knows the
agent; it should offer to bring the pane back.

## The reconciler already knows how, and has stopped looking

`plan_panes` models the desired state as EXACTLY one pane per roster
label, master included: a label with no live pane goes in `add`, a
missing master is added and then staged by `relocate`. Every roster
change converges through it, and a pass verifies placement by reading
the panes back.

What it does not do is look again. `reconcile` returns early when the
`RosterView` (labels + master) is the one it last VERIFIED converged.
A pane closed by hand after that changes nothing in the view, so the
reconciler never re-lists and the gap stays. That is the whole
problem: not a missing capability, a cache with no invalidation the
user can reach.

## Change

A **reopen pane** item on the agent's action menu in `clank status
--tui`. Choosing it sends the worker a request that CARRIES THE LABEL,
on the same channel roster changes travel. The worker takes one
listing, runs `plan_panes` over it, and acts on that one label only:
if it is in `add`, the pane is created through the existing path (born
stacked when the stack is clean, reconciled otherwise; a master is
added and then staged), and placement is read back. If it is not in
`add`, nothing is created and the answer is "already open".

The item is per-agent, so the pass must be too. A full pass adds EVERY
missing roster label, and two panes can be missing at once; reopening
one must not launch the other, which the user did not ask for (codex
on 30ff9f3). So the request is a one-shot outside the convergence
machinery: it never touches another label's pane, never closes
anything, and leaves the converged cache as it found it — a targeted
add cannot make a stale cache wrong in a new way, and letting the
generic retry loop see the pass would have it add the rest on the next
round, the same leak one step removed. Roster changes still converge
the whole tab exactly as today.

No new spawn path: `open_zellij.rs` stays the only spawner and the
reconciler the only pane owner.

**One-shot does not mean lease-free.** Structural pane work — list,
create, stack, relocate — runs under the reconciliation lease
(`PaneIo::may_reconcile`, the flock on
`.clank/zellij-reconcile.lock`) precisely because list-then-create
from two TUIs is the documented race: both see the label missing,
both create, the tool refuses the second with "already has an active
writer", and a corpse the listing counts as that label's pane is left
behind. A reopen IS list-then-create. So the targeted handler asks
`may_reconcile` first and, denied, takes no snapshot and touches no
pane; the status line says another status TUI holds this repo's
panes and to reopen from there (codex on 70df3aa). "Outside the
convergence machinery" means only what it said: the request does not
enter the retry loop or the converged cache.

The item is present on every agent, not only when the pane is
believed missing. Whether a pane exists is exactly the fact the cache
can no longer be trusted about, and one on-demand listing is the
honest answer. Choosing it for an agent whose pane IS live is a pass
that finds nothing to add and says so. The TUI shows the outcome in
its status line: reopened, already open, not in a zellij session, or
zellij did not answer.

## Tests

- Through the worker message the TUI sends (not by poking reconciler
  fields): with a fixture listing missing TWO panes, reopening one
  adds and stacks exactly that pane, the other stays absent, and the
  status line reports the chosen label's result.
- Missing the master, the request adds it and relocates; missing
  nothing, it adds nothing and reports already open.
- A converged reconciler stays converged across the request — a
  following roster snapshot with the same view does not trigger a
  full pass because of it.
- The item appears for master and reviewers alike (`detail_actions`).
- Outside zellij the request reports that and touches nothing.
- With the lease denied (`may_reconcile` false), the request takes no
  snapshot and makes no pane call — the fixture `PaneIo` counts both —
  and the status line names the other holder.

## Out of scope

- Restarting an agent whose pane is alive but whose process exited;
  that is the ✗ indicator + manual respawn story.
- Reopening a pane in a tab other than the repo's own.
