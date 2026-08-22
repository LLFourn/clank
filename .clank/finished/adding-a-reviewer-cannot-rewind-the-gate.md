# adding-a-reviewer-cannot-rewind-the-gate

## Why

Adding a reviewer during master's turn takes the turn away and hands it
back to the reviewers, for a commit the new reviewer was never present
for.

`tier_coverage` (`core/src/wait.rs:760`) resolves the expected set
against TODAY's roster and marks any member with no entry as `pending`:

    None => pending.push(label.clone()),

So the moment a label joins the roster it acquires a pending slot on
the latest commit, retroactively. A `Finished` plan falls back to
`ContinuedPendingGate`; a `Continued` one falls back to `Unreviewed`;
master stops and the newcomer is woken to review work it did not see.

The roster is being read as if it had always been the roster. Who
reviewed a commit is a historical fact, but the fold infers it from
present configuration, which makes every past gate state a function of
today's config — so any widening rewrites history.

## The fix: record the fact instead of inferring it

Materialise the newcomer's non-participation as a recorded verdict for
the latest reviewable commit, so the fold stops inferring a pending
slot that never existed.

**The verdict is whichever leaves the gate unchanged** — not a
hardcoded CONTINUE. A CONTINUE stand-in on a FINISHED plan still
demotes it to `ContinuedPendingGate`, which is the same rollback in a
smaller coat. The rule is one line:

> Adding a label to an expected reviewer tier must not change the
> latest commit's gate state beyond the effects of labels removed by
> the same transition.

For a transition that adds an expected reviewer, compute the baseline
from the post-transition roster with that entrant omitted, then add the
entrant and choose a distinguishable stand-in only if one is required
to retain that baseline gate. This also decides WHEN to write one. If
the pending slot already leaves the gate unchanged, no stand-in is
written and the newcomer reviews normally. In particular, there is no
enumeration of supposedly "open" states: a commit-tier add during
`ContinuedPendingGate` does rewind to `Unreviewed`, so the invariant
requires a stand-in there even though review is not finished.

## Every widening path, one seam

Put this at the repo-roster WRITE choke point, not in a list of command
handlers. Replace direct roster persistence with one transition helper
that receives the before and after configs and whose policy is explicit
in its type:

- ordinary roster transitions preserve the baseline gate against every
  newly expected reviewer;
- identity substitution deliberately requires fresh review.

Every roster writer must use that helper, so a future command cannot
persist a widening without choosing a policy. This covers both add
forms, `set_repo_review`, and the already-existing fifth path,
`set_repo_master`: promotion demotes the previous master into the
commit-review tier, adding a new expected reviewer to the latest commit.
The narrowing part of a tier change or promotion may legitimately
resolve a gate; only the newly introduced pending slot is neutralised.

`swap_repo_agent` is the deliberate exception, expressed through the
same seam rather than bypassing it. `agent-swap-preserves-role-and-pane`
settled that the departed reviewer's verdict is void because a verdict
is an agent's judgement, not the role's. A swap therefore writes NO
stand-in for the incoming label and intentionally re-opens the gate for
a fresh review. This plan does not supersede that decision.

## The record must not pass for a review

The file lands at the normal
`agents/<label>/feedback/<commit>.md` path
(`disk_format.rs:60`), so everything that reads feedback sees it. It
must therefore say what it is, in the body and in a form code can test
— an agent that never read the commit must not appear to have approved
it in `clank log`, the HTML, or a reviewer's own history.

It must also never overwrite an existing verdict. Re-adding a label
that was previously removed, and had genuinely reviewed, keeps the real
review.

## Required tests

- For EVERY gate state, adding a reviewer leaves the latest commit's
  gate identical — the invariant, asserted directly as
  `gate_before == gate_after`.
- Where adding a pending slot already preserves the baseline gate, no
  stand-in is written and the newcomer remains pending.
- A commit-tier entrant during `ContinuedPendingGate` does not rewind
  the gate to `Unreviewed`.
- A gate-tier entrant does not demote a FINISHED plan.
- Promoting a reviewer does not let the demoted former master rewind the
  latest gate merely by entering the commit-review tier.
- `set_repo_review`, promotion, add, remove, and swap all persist through
  the one transition seam, not command-local copies.
- Swap writes no stand-in and still requires a fresh review from the
  replacement, preserving
  `a_departed_reviewers_verdict_does_not_satisfy_the_gate_for_its_replacement`.
- Only the latest reviewable commit gets a stand-in; older commits are
  untouched.
- An existing real verdict is never overwritten.
- The stand-in is programmatically distinguishable from a real review.

## Out of scope

- Removing a reviewer. A pure narrowing can only resolve a gate, never
  rewind it, and needs no record. The transition seam still observes the
  write so a combined role change can handle any additive component.
- Back-filling older commits. Only the latest is gated, and writing
  history for the rest would be a lie at scale.
- Display of stand-ins in `clank log` / HTML beyond being
  distinguishable. Worth doing, not needed for the gate to stop
  rewinding.
