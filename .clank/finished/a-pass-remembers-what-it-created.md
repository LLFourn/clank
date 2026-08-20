# a-pass-remembers-what-it-created

## The claim this plan retracts

`zellij-pass-is-cheap-and-cannot-double-add` concluded, and committed,
that "a single driver cannot double-add: its worker is one thread
draining an in-process mpsc::Receiver, and reconcile has exactly one
production caller on that thread." A reconcile lease was built on that
reasoning, to stop the OTHER case.

It is false, and the counter-example is live. Adding `ruthless` to
`chain-redesign` produced TWO panes with identical launch commands
(`terminal_214`, retitled `👀 ruthless (reviewer)`, and
`terminal_215`, still bare) while:

- exactly ONE driver existed for that repo (`clank status --tui`
  pid 64381, started 22:44:06);
- it was running the fixed binary (installed 13:54, well before);
- and it HELD the lease — `.clank/zellij-reconcile.lock` exists,
  created 22:44, matching the driver's start.

So the lease worked, and the double-add happened anyway.

## Why the reasoning was wrong

Serialization prevents CONCURRENT passes. It does not prevent a LATER
pass from acting on a listing that does not yet show the earlier
pass's creation.

Each pass re-derives `plan.add` from a fresh `io.snapshot()`. The ids
it creates land in `created_reviewers`, which is used for the one
`io.stack(...)` call and then dropped. Nothing carries a created label
into the next pass's idempotence check, and that check —
`find_pane_by_command` over the pass-start snapshot — is the only
thing standing between a roster label and a second `new-pane`.

The lease is still correct and stays: it closes the cross-process
case, which is real. It simply was not the whole cause, and the plan
should not have claimed the single-driver case was closed by
reasoning alone when the failure was reproducible.

## Approach

1. **The reconciler remembers what it created until a listing confirms
   it.** Keep `label → created pane id` across passes; treat a
   remembered label as present for `plan.add`; drop the memory once a
   snapshot reports that pane.

2. **The memory must expire, or a failed creation is permanent.** If
   `new-pane` reported an id that never appears in a listing, an
   unbounded memory means that label is never created again. Decide
   the discharge condition and state it — a bounded number of passes,
   or dropping the memory the first time a listing omits it after a
   confirming read. This is the same hazard class as the bounded
   repair, so it must be explicit, not implied.

3. **Do not paper over it with a delay.** Sleeping after `new-pane`
   would trade a correctness fix for a timing guess and slow the pass
   this repo just spent a plan making cheap.

## The state it settles into, measured

Minutes later, tab 29 holds:

    id=214  ruthless  exited=False  '👀 ruthless (reviewer)'
    id=215  ruthless  exited=True  held=True  status=0  'ruthless (reviewer)'
    id=212  codex     exited=False  '💤 codex (reviewer)'   (right column)
    id=211  claude    master                                (left column)

Both ruthless panes keep their `terminal_command`, so the guard CAN
see them; the duplicate is not caused by an unidentifiable pane.

**A duplicate with one copy exited is exactly what
`remove_target_ids`' exited-first preference was built for, and it is
not running.** That is the finding worth acting on, and it looks
structural rather than incidental.

`plan_panes` should read ruthless twice, budget one removal, and close
the EXITED copy. It has not. The leading explanation — to be confirmed,
not assumed — is that the pass exhausted `MAX_FAILED_PASSES` and
cached convergence: the reviewers are unstacked, so the verifying read
fails every pass, and after the bound the reconciler stops acting
ENTIRELY.

If that holds, one budget covers two different kinds of work:

- placement repair, which genuinely can be unfixable (a lone reviewer
  sharing the instrument pane's stack cannot be un-stacked), and
- multiplicity, which is always fixable — closing a pane never fails
  the way stacking does.

Sharing a budget means an unfixable stacking problem STARVES the
fixable duplicate removal, and the duplicate is itself part of why the
tab will not verify. The bound is right; applying it to both is the
suspected error. Establish whether that is what happened, and if so
separate the budgets rather than raising the bound.

## Established: the shared budget is NOT what stranded the duplicate

The plan asked for this to be confirmed, not assumed. Traced, and the
answer is no — so the budgets are deliberately left shared.

`MAX_FAILED_PASSES` is 2, and BOTH the budget and the convergence cache
are keyed on the same `RosterView`. Any two failing passes on one roster
abandon the tab regardless of which kind failed, and the moment the
budget spends, `converged` is cached and every kind stops together. So
attributing failures to two counters changes no reachable outcome: there
is no pass sequence where a split budget removes a pane the shared one
leaves. A refactor with no behavioural difference and no test that can
distinguish it is not worth the extra state.

What actually stranded it is the double-add plus that same cache:

    pass N    roster gains `ruthless` → snapshot lacks it → create (214)
    pass N+1  snapshot STILL lacks it → create again (215); the verify
              now sees two → not converged → failure 1
    pass N+2  snapshot shows two → remove issued → the tab is still
              unstacked, so the verify refuses → failure 2 → SPENT →
              `converged` cached with both panes live

The budget spent on a failure the lagging listing caused. Remembering
the creation removes pass N+1 entirely, so the duplicate never exists
and the budget is never spent on it.

The exited-copy preference in `remove_target_ids` was therefore never
reached, not broken. Whether `close-pane` can dismiss an EXITED HELD
pane (`terminal_215` was `exited=True held=True`) is a separate,
still-open question — it is what a manual `clank open` would have to
clear today. Not in scope here.

## Second defect in the same report

The two panes are also NOT stacked with `codex`: both sit in the left
column with the master, `codex` holds the right column. Establish
whether the anchor was found and `stack-panes` ran and failed, or the
duplicate broke stacking (three ids for two labels, one of them
destined for removal), or the bounded repair had already given up —
`MAX_FAILED_PASSES` correctly caps repairs, so "wrong and left alone"
is an expected END state and must not be mistaken for the cause.

Also determine why multiplicity did not close the excess pane: two
panes for one in-roster label is exactly what `plan_panes`' remove
budget exists for, and the tab still shows both.

## Required tests

- A pass whose snapshot does NOT yet show the previous pass's creation
  does not create a second pane for that label.
- The memory's discharge condition, asserted both ways: it clears when
  a listing confirms the pane, and it does not strand a label forever
  when the pane never appears.
- The duplicate-and-unstacked shape from this report converges: the
  excess pane is closed and the survivor joins the reviewer stack.
- No test spawns zellij or an agent binary.

## Out of scope

- The reconcile lease, which is correct for the case it covers.

## Observed while writing this: a declined promote has no discharge

The `promote` wake states its discharge condition as "fires until the
item is promoted or removed from the queue". Both discharges are
ACTIONS ON THE QUEUE. There is no discharge for the third real
outcome: the human declined the promotion and the item should wait.

So a queue item the user has said no to wakes its master forever, and
the only ways to stop it are to do the thing that was declined or to
throw the plan away. The agent's correct move — hold — is the one the
wake cannot represent, which is precisely the "nudged in a loop with
no way to stop it" complaint that motivated
`wakes-state-their-discharge-condition` and `attending`.

Worth its own plan rather than this one. A deferral that the wake
respects (and that expires, so a plan cannot be silently buried) is
the shape to consider.
