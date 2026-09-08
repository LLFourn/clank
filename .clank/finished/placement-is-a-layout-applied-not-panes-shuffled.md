# placement-is-a-layout-applied-not-panes-shuffled

> When I switch a reviewer to master and do alt+[, the new master's
> pane is renamed "codex (reviewer)" and put in the reviewer stack. I
> thought the system was: watch for config changes → produce the
> layout from the new config as a pure function. Then alt+[ should do
> the same, just under a different orientation. — lloyd

That IS the right model. Clank does not implement it, and the bug is
the gap between the two.

## What clank does today, and why alt+[ undoes a promotion

The layout is a pure function of the roster exactly once: at `clank
open`, `compose_kdl` writes the tab layout AND two `swap_tiled_layout`
variants (landscape/portrait) into the file zellij is launched with.
The variants pin the STAGE to the master's launch command — `pane
size="65%" name="claude (master)" { command "clank" args "agent start
claude --repo …" }` — because zellij matches existing panes to swap
slots by that command; the reviewer region is a `children` slot that
takes whatever else is present
(a-swap-layout-describes-a-shape-not-a-roster).

After that, roster changes are followed by the TUI's `PaneReconciler`
SHUFFLING panes with actions: `new-pane` for a missing label,
`close-pane` for a departed one, `stack-panes` to rebuild the reviewer
stack, `move-pane` to relocate a new master to the stage, a bounded
repair loop for stacks that will not form, and a retitler. Zellij's
copy of the layout — the swap variants alt+[ / alt+] apply — is never
touched again, because until now there was nothing that could touch
it.

So: promote codex. The reconciler moves codex's pane to the stage and
retitles. Press alt+[. Zellij re-applies the variant written at open,
whose stage slot matches ONLY the pane running `clank agent start
claude`; claude goes back to the stage, codex falls into `children`,
and the arrangement reverts to the roster of an hour ago. The
retitler then labels what it sees.

Two sources of truth for placement — clank's shuffling and zellij's
frozen variants — and every roster change makes them disagree.

## The primitive that makes the model real

zellij 0.45 has `zellij action override-layout`. Measured on this
machine with a throwaway session (the probe scripts are not kept),
because every claim below is one the reconciler will depend on:

- `override-layout --apply-only-to-active-tab
  --retain-existing-terminal-panes <layout>` re-applies a layout to a
  LIVE tab: existing panes are matched to slots by launch command, no
  pane is spawned when every command slot has a live match, and
  geometry follows the layout — after applying a layout with beta on
  the stage, beta's pane (id unchanged) sat at 65% width and alpha
  joined the stack.
- **The layout's `swap_tiled_layout` blocks replace the tab's.**
  `previous-swap-layout` after the override flipped to portrait with
  beta still on the stage; `next-swap-layout` flipped back. This is
  the fix: regenerate, override, and alt+[ applies the CURRENT roster
  at the other orientation.
- A user's own extra pane (a shell opened by hand) is retained and
  lands in the reviewer stack's `children`, as it does under a swap
  today. Nothing of the user's is closed.
- Panes are not renamed by the override. Titles stay the retitler's,
  from the roster, as today.
- **A command slot with NO live match breaks the matching**: with a
  slot for a `delta` nobody had launched, zellij spawned a SECOND
  `beta` for the stage and dumped every existing pane into the stack,
  and did not spawn delta. So the layout applied must name exactly
  the panes that are RUNNING — missing labels are launched first and
  the layout composed after.
- Without `--apply-only-to-active-tab` the override reshapes the whole
  session to the layout's tabs and CLOSED the other tab. The flag is
  not optional, and the target tab must be the active one: focus it
  by id, override, restore the previously active tab and the
  previously focused pane. The other tab survived that dance
  (measured with `go-to-tab-name`; the implementation uses the id).
- The override moves pane focus within the tab; the pass's existing
  focus transaction (capture once, restore once) covers it.

## The model, implemented

`PaneReconciler`'s pass becomes the sentence lloyd wrote:

1. **Membership**, exactly as today. Close panes whose label left
   the roster. Open a pane for each roster label with NO pane —
   `new-pane` with the agent's launch command, so its command is its
   identity. An EXITED pane still counts as its label's (`pairs`
   already says so): a crashed agent is the ✗ indicator and the
   operator's manual reopen, never an automatic relaunch — a crashing
   agent restarted on every refresh is the loop that design refuses
   (codex on 40760e0). Then list again: the layout below may only
   name what is running.
2. **Layout.** `compose_kdl(roster ∩ running, user_template,
   orientation)` — the same pure function `clank open` uses, with the
   SAME template source: `~/.clank/config.json#/zellij/layout` when
   the user set one, else the built-in. The built-in yields master on
   the stage, reviewers in the stack, instrument pane, BOTH swap
   variants; a user template yields the user's chrome around the
   agent group at the marker and NO generated swap variants, exactly
   the contract `clank open` gives it — a workspace opened from a
   marker template is reconciled with that template, never with the
   built-in geometry (codex on aa30817). The template is read at each
   pass, so an edit to it is picked up by the next roster change.
   Applied with `override-layout --apply-only-to-active-tab
   --retain-existing-terminal-panes` to the repo's tab. Only RUNNING
   panes are named: an exited reviewer is left out and is retained
   by the override as an unmatched pane, in the stack, wearing its ✗
   until reopened. **No running master, no override**: the layout has
   no stage to give, so the pass does membership only and stops; the
   targeted `reopen` that brings the master back runs the override
   itself. Orientation is the tab's CURRENT one, read from the
   listing's geometry (the stage beside the instrument pane is
   landscape, above it is portrait), so a user who flipped stays
   flipped; a fresh tab falls back to the size rule.
3. **Verify** by listing: every roster label present, the master's
   pane the widest/tallest in its axis, reviewers sharing the stack
   region. Converged is cached only on a verified pass, as today.
4. **Retitle** from the roster, as today.

**The failure budget stays, and widens.** Any step — an add, a close,
the override, the verifying listing — can fail, and a reconciler that
caches only verified convergence would then act on every refresh
forever: exactly the unbounded side-effect loop the current code
bounds with `failed_repairs`/`MAX_FAILED_PASSES` (codex on 40760e0).
The budget is kept and applied to the WHOLE transaction: a pass that
does not verify spends one; at the limit the roster is cached as
converged and left alone until the roster changes, as today. The
lone-reviewer carve-out (`reviewers.len() < 2`) goes, because the
limitation it excused — `stack-panes` cannot unstack — no longer
exists.

What leaves: `stack`, `relocate`, `is_placed`, the placement-repair
branch that ran them, the "stack-panes cannot unstack a pane"
limitation and its carve-out, and the anchor bookkeeping that existed
to feed `stack-panes` an id. The layout does all of it in one action,
and it cannot disagree with the swap variants because it IS them.

What stays: the lease (`may_reconcile`), the failure budget, the
pending-creation beliefs, `confirm_listed`, the targeted `reopen`
(which now ends in the override rather than a stack/relocate), the
retitler, the one-listing-per-pass rule, and the caller pane
exclusion.

`open_zellij.rs` stays the sole spawner: `override_tab_layout(session,
tab_id, kdl)` joins the other actions there, with the focus dance
inside it. The tab is addressed by its STABLE ID — `go-to-tab-by-id`
with the `tab_id` the listing reports for the repo's panes — never by
name: zellij allows duplicate tab names, and two worktrees can share
one (codex on 40760e0).

## Version floor

`override-layout` is 0.45. `PlacementCapability` probes by
capability, not version string; it now probes `override-layout
--help`, and `ClientTooOld` means "reviewer panes are not placed and
alt+[ is stale until you upgrade" — the doctor says so. The
shuffling code is not kept as a fallback: two implementations of
placement is the disease this plan treats.

## Tests

The fake `PaneIo` gains `override(kdl)` and records the KDL it was
handed; assertions are on that KDL through the real `compose_kdl`:

- Promotion: after the roster's master changes, the override's stage
  slot names the NEW master's launch command and the old master is
  absent from it (it is a `children` occupant), and BOTH swap
  variants in the same KDL pin the new master — the assertion that
  answers the report.
- A missing label is opened BEFORE the override, and the override's
  KDL names only labels that were live after the add (the duplicate
  spawn measured above cannot be reached).
- A departed label is closed before the override, and the KDL does
  not name it.
- An exited reviewer is NOT closed, NOT reopened, and NOT named by
  the override — it stays, and the retitler keeps its ✗. An exited
  master: membership runs, no override is issued, and the pass ends;
  a `reopen` of the master issues one.
- Bounded failure: an override the fake reports as failed, or a
  verify that never confirms, spends the budget and after
  `MAX_FAILED_PASSES` the roster is left alone — no override on the
  next refresh — until the roster changes, when the budget resets.
- Orientation follows the tab's geometry: a portrait listing yields a
  portrait main layout; an empty tab yields the size rule's.
- Under a configured marker template, a roster change applies THAT
  template with the new roster: the override's KDL carries the
  template's own chrome, the new master's launch command at the
  marker's stage, and no `swap_tiled_layout` block — and never the
  built-in layout's. Under no template, the built-in with both
  variants.
- A user's extra pane is never named by the override and never
  closed (the fake records closes).
- `override_tab_layout`'s argv is `--apply-only-to-active-tab` and
  `--retain-existing-terminal-panes`, the tab is focused BY ID before
  (`go-to-tab-by-id`, never `go-to-tab-name`) and the previous tab
  and pane refocused after — asserted on the recorded action
  sequence, with a listing in which two tabs share a name and only
  the id picks the right one.
- The capability probe: `override-layout --help` failing reads as
  `ClientTooOld`, and the doctor names alt+[ in its warning.
- Every reconciler test that asserted `stack`/`relocate` calls is
  rewritten against the override KDL; none is deleted without its
  behaviour reappearing as a layout assertion.

Mutation-checked with production-only edits: the stage pinned to the
old master; the override run before the add; the retain flag
dropped; the orientation read ignored.

## Out of scope

- Generating swap variants for user templates. `clank open` never
  did (a template author writes their own swaps), and the override
  keeps that contract; changing it is a different plan.
- Moving panes between tabs; 0.45 has no such action.
