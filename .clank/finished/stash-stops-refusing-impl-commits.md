# stash-stops-refusing-impl-commits

`clank stash push` refuses on any plan that has been implemented.
Make it just work.

## What happens

Stashing a plan with implementation commits fails with:

```
stash push refuses: commit(s) <shas> touch non-plan content that
would be set aside with the plan. Pass `--force` to stash them anyway.
```

Hit for real on `zellij-pane-placement-and-cost`, whose four commits
are ordinary implementation work for that plan.

## Why the guard is wrong

`safety_check` (cli/stash.rs) is tiered:

1. FOREIGN commits (another plan's, interleaved) → unconditional
   refusal, `--force` does not bypass. Correct and worth keeping:
   you cannot set aside someone else's plan as a side effect.
2. Non-foreign commits whose disposition is `Rewrite` → refuse
   unless `--force`. **This is the broken tier.**
3. Everything `Drop` → proceed.

A commit classifies as `Rewrite` when it touches content beyond the
plan document — which is what an implementation commit IS. So tier 2
fires for every plan that got past its intro commit, and the whole
point of stash is to set a plan's work aside and pop it back later.
The guard treats the normal case as the dangerous one, and the
"safety" it offers is a prompt to pass `--force`, which teaches the
operator to force by reflex — the opposite of a safety measure.

The comment says the tiering was inherited verbatim from `demote`.
There is no `demote.rs` any more, and nothing else calls
`safety_check` — it is stash-local, so this can be changed without
touching a shared contract. (An earlier draft of this plan claimed
demote shared it; that was wrong.)

The decisive precedent is `purge`. `purge --drop` PERMANENTLY
deletes a plan and its commits, and it deliberately does NOT enforce
the Rewrite tier — `drop_safety_check` (cli/purge.rs:389) mirrors
only the foreign arm, reasoning that "the `--drop` flag itself is
the opt-in to losing code".

So the IRREVERSIBLE operation waves implementation commits through,
while the REVERSIBLE one (stash pops back) demands `--force` for
them. The risk calibration is exactly inverted, and stash is the one
that should be relaxed rather than purge tightened.

## The hazard, and why nothing cheap detects it

There IS a real one: a commit carrying the plan's work AND unrelated
drive-by changes takes the unrelated part with it, silently removing
it from the branch.

An earlier draft claimed the commit TAG distinguishes that. It does
not (codex on 6242f94). A `[plan]` tag is an ownership ASSERTION
about the whole commit, not proof that no unrelated file changes are
mixed in — the tagging rule says which plans a commit touches, not
that it touches nothing else. So tier 2 cannot be replaced by a
smarter predicate; there is no cheap signal here.

What remains is a deliberate trade, and `purge` already made it: the
IRREVERSIBLE `--drop` waves these commits through on the reasoning
that the flag is the opt-in. Stash is reversible, so accepting the
same trade costs strictly less — a mixed commit that goes into the
stash comes back on pop.

## Untagged commits are ALREADY refused — no new decision to make

The earlier draft proposed keeping the foreign arm unchanged while
separately choosing warn-or-proceed for untagged ad-hoc commits.
That is incoherent: `foreign` is `!attributed`, where attribution is
membership in the plan's own timeline (preview.rs:225), so an
untagged commit inside the range is ALREADY foreign and already
refused unconditionally.

Decision: leave it that way. Untagged work interleaved in a plan's
range keeps refusing, `--force` keeps not bypassing it, and this
plan introduces no three-way attribution model. That is the
conservative half of the change, and it is what makes deleting tier
2 safe to reason about: the commits tier 2 blocks are, by
definition, ones this plan's own timeline claims.

The cost, stated so it is not discovered later: an ad-hoc commit
landing mid-plan still blocks stashing until it is disentangled,
exactly as today.

## Scope

- Delete tier 2. Stashing a plan's own implementation commits is the
  normal path and must need no flag.
- Leave untagged/foreign handling exactly as it is (see above), and
  make sure the code says WHY the two tiers differ, so the next
  reader does not "restore symmetry" by adding tier 2 back.
- Keep tier 1 exactly as it is.
- Re-check whether `--force` still has a job. If nothing reaches it,
  remove the flag rather than leaving a lever wired to nothing.
- Consider whether stash should simply adopt `purge`'s
  `drop_safety_check` shape (foreign-only). Two near-identical
  checks that differ by one tier invite the next divergence; if
  they end up the same rule, they should be the same function.

## Acceptance

- Stashing a plan with ordinary implementation commits succeeds with
  no flag, and `stash pop` restores them — asserted end to end
  through the real cores, not by unit-testing the predicate.
- A plan whose range contains another plan's commit still refuses,
  and `--force` still does not bypass it.
- The foreign refusal keeps its coverage, including that `--force`
  does not bypass it — deleting tier 2 must not weaken tier 1.
- An UNTAGGED ad-hoc commit interleaved in the target plan's range
  still refuses, end to end, and `--force` does not bypass it (and
  if `--force` is removed entirely, it still refuses). This is a
  DISTINCT source of foreign status from another plan's commit:
  that case proves a tagged-elsewhere commit is refused, not that an
  untagged one is. Deciding untagged stays refused (see above) is
  only half a decision until a test would fail when a later
  attribution refactor quietly makes such commits attributed.
- The `zellij-pane-placement-and-cost` stash that prompted this
  works: `clank stash push <plan>` with no flag, `pop` brings the
  four commits back.
