# promote-forces-self-review-continue
# promotion auto-continues the demoted master's self-review demands

The edge case (lloyd, 2026-07-13): agent A authored commit ONE as
master; agent B (reviewer) is promoted. Under the new roster A is a
reviewer, so the gate now waits on A's review of commit ONE — a
commit A itself authored. Technically consistent, humanly absurd:
the roster flip manufactured a self-review demand, and the plan
stalls until A rubber-stamps its own work.

## Most of this landed while the plan was stashed

`adding-a-reviewer-cannot-rewind-the-gate` shipped the roster-stand-in
machinery, and a promotion is one of the roster widenings it covers:
`set_repo_master` already routes through `write_repo_transition` with
`RosterTransitionPolicy::PreserveGate`, and
`promotion_neutralises_the_demoted_masters_new_pending_slot` pins
exactly the behaviour the next section describes. The demoted master
entering the commit tier already gets a synthetic CONTINUE, written
before the flip, skipped when a real review exists, and rolled back if
the config write then fails — the last of which is more than this plan
asked for.

So the section below is a record of the design, not of work still to
do. What actually remained:

- `clank rereview`, the escape hatch — nothing existed.
- The stand-in body said nothing about it. A synthetic verdict nobody
  is told about is one nobody knows to question, which defeats the
  point of having an escape hatch at all.
- Nothing announced a forced verdict as it was written.

## Fix — at PROMOTE time, in the shared core

When `set_repo_master` flips the roster (covering both `clank agent
promote` and the TUI's promote action, which route through it):

- Compute the DEMOTED agent's pending reviewer items under the NEW
  roster (the same projection `wait` uses). Every item whose commit
  ALREADY EXISTS at promote time is a review the promotion itself
  manufactured — under the single-master model those are commits the
  demoted agent authored as master. Roster history is local-only, so
  tenure is not derivable; "pending at the flip" is the detection
  rule, stated honestly. (Chained promotions can sweep in a
  predecessor's commit — the forced note names the mechanism, so the
  new master can re-trigger a real review; see below.)
- For each, MECHANICALLY write the feedback file (the same typed
  write `clank feedback write` uses — never raw paths), author = the
  demoted agent, verdict = CONTINUE, body explaining the situation:

      CONTINUE auto-continued at promotion: <A> authored this commit
      as master and cannot meaningfully self-review it after being
      demoted to a reviewer.

      New master: the roster flip, not a human judgment, produced
      this verdict. If you believe this work is actually COMPLETE,
      open a fresh review round: run `clank rereview <plan>`.

- **`clank rereview [plan]`** (new): rewrite the plan's latest
  reviewable commit through the EXISTING rewrite engine
  (`rewrite::reword_in_place` — same message, same tree), which
  already handles the BURIED-tip case by
  replaying descendants (codex 7af14b6: ad-hoc or another plan's
  commits can sit above this plan's tip, and a naive ref move would
  drop them). The engine runs through clank's plumbing, NOT git's
  CLI, so no post-rewrite hook fires; clank migrates feedback for
  the returned (old → new) pairs itself — for every DESCENDANT pair
  as normal, while DELIBERATELY excluding the target pair (lloyd's
  hookless-amend insight, applied through the engine): the rewritten
  target carries zero feedback and the WHOLE current roster —
  including the demoted agent, now legitimately — gets a pending
  review. The old sha's feedback stays behind, inert. The
  tag/touched invariant holds by construction (identical tree +
  message revalidate exactly as before), and the target remains the
  plan's latest reviewable sha.

  (A raw `git commit --amend` cannot do this: the post-rewrite hook
  migrates EVERY feedback file — the synthetic one included — onto
  the amended sha by rewire's deliberate invariant, so a hooked amend
  re-opens nothing. An empty tagged commit fails the
  tagged-but-not-touched invariant, and an untagged one is ad hoc —
  codex 77a563b, 784ed2a. The hook-free engine rewrite with the
  target pair excluded from migration is the one shape that opens a
  fresh round without violating an existing invariant, and it
  inherits the engine's existing safety refusals — dirty tree,
  protected-branch rules — for free.)

### Rereview must work on the branch plans actually live on

`reword_in_place` refuses a protected branch (`main`/`master`) unless
`allow_rewrite_protected` is set — and active plans, including the
promotion handoff this plan exists to unstick, normally sit exactly
there. This repo's own branch is `master`. So "inherits the engine's
protected-branch refusal" made the generated instruction unusable in
the common case: the new master would be told to run a command that
refuses (codex on b15082e).

`rereview` therefore authorizes the protected in-place rewrite
explicitly. That is a deliberate widening, not an oversight to be
inherited quietly: this command exists to rewrite the plan's own tip,
the engine's other refusals (dirty tree, merge commits, target off the
first-parent chain) still apply, and the acceptance exercises the
end-to-end reset ON a protected default branch rather than on a side
branch where the refusal never fires.

### A same-message reword does NOT reliably mint a new sha

Measured, not assumed. `reword_in_place` writes its target with
`git_plumbing::squash_commit`, which pins the committer date to the
AUTHOR date so that re-squashing is idempotent
(`finish-squash-idempotent-on-finished`). Rewording a commit that
`squash_commit` itself wrote, with an unchanged message, therefore
reproduces a byte-identical object — the same sha:

    reword #1 on an agent-authored commit → sha changes
    reword #2 on the commit #1 produced   → SAME SHA

So `rereview` would work once per plan and then silently do nothing,
which is worse than failing outright: the new master runs it, sees no
error, and the review round never opens. The first success is
incidental — it comes from the original commit's committer metadata
differing from what `squash_commit` writes, not from any guarantee.

The fix is a committer timestamp that is DETERMINISTICALLY different,
not merely probably different:

- The rereview target keeps its tree, parent, author and message, and
  takes a committer time derived to be strictly greater than the old
  target's — `max(now, old_committer_time + 1s)`. Squash idempotency is
  a real property worth keeping, so this is an opt-in on the reword
  path rather than a change to `squash_commit`.
- Ambient-`now` alone is NOT enough, and this is where a first attempt
  goes wrong twice over. It collides at one-second resolution, so a
  rereview in the same second as its target silently no-ops; and
  bolting a post-hoc equality error onto it converts that silence into
  a spurious FAILURE — two rereviews in quick succession would make the
  second one error, contradicting the acceptance below and teaching
  users to retry on timing (codex on b15082e). Deriving from the old
  commit's own time removes the wall clock from the guarantee
  altogether.
- The sha-inequality check stays, as a DEFENSIVE assertion rather than
  the mechanism. It should now be unreachable; if it ever fires,
  something about the object model changed and silently no-oping would
  be the worse answer.

- The forced CONTINUE leaves the gate un-finished by design: the new
  master wakes with the state visible (the forced review renders in
  the log like any other), and either keeps implementing or runs
  `clank rereview` to open a genuine round. No special finish-path
  handling.
- Promotion output names what it forced: `auto-continued <sha> for
  <A> (authored as master)` per item, so the human sees it too.

## Atomicity

The forced feedback files are written BEFORE the roster flip: while
the demoted agent is still master they sit inert (a master is in no
review tier, so the gate never reads them). Any write failure aborts
the promotion cleanly — roster untouched, at worst inert files left
behind (harmless dead weight, named in the error). Only after every
forced file lands does `set_repo_master` flip the roster; the flip
itself remains the single atomic config write it is today. No path
reports failure after a partial master flip (codex 77a563b).

## Care points

- Do NOT overwrite an existing review by the demoted agent for that
  sha (they already reviewed → nothing to force).
- No pending items (fresh repo, or the demoted agent isn't in a
  reviewing tier under the new roster) → promote behaves exactly as
  today.
- `agent set-review` and `agent add` do NOT force anything — only the
  master flip manufactures self-reviews.
- The forced file must survive the reviewer's own armed wait: after
  the write, the demoted agent has no pending item for that sha (its
  wait no longer wakes for it) — that's the point.

## Acceptance

- In-process promote test: A master authors an intro commit; B
  promoted → A's feedback file for that sha exists with CONTINUE and
  the explanatory body; A's wait projection shows no pending review;
  the gate reads continue-not-finished.
- `rereview` run TWICE IN IMMEDIATE SUCCESSION yields a different sha
  each time. Both halves matter: the second run is the one a pinned
  committer date silently no-ops, and running them back to back is
  what a wall-clock-dependent fix fails. No sleeping in the test —
  needing one would be the bug.
- The reset works on a PROTECTED default branch (`master`), which is
  where plans and promotions actually live.
- The defensive sha-inequality check reports rather than no-ops.
- The RESET PATH IS REAL, pinned end to end through the projection
  (codex 784ed2a): after the forced continue, `clank rereview` yields
  a new sha that passes HEAD tag validation, and derive_status/
  work_for report pending reviews for the FULL new roster — demoted
  agent included; the old sha's feedback demonstrably did not
  migrate.
- BURIED-tip fixture (codex 7af14b6): with an ad-hoc commit above
  the plan tip, `rereview` preserves the descendant (its tree and
  its own genuine feedback, migrated to its replayed sha) while the
  rewritten target carries none.
- `clank rereview` refuses when the plan has no reviewable tip and
  inherits the engine's dirty-tree refusal (pinned by the engine's
  existing tests; one smoke assertion here).
- Failure injection: a forced-feedback write failure aborts the
  promotion with the roster unchanged.
- Existing-review guard: A already reviewed the sha → promote leaves
  the file untouched.
- No-pending promote byte-identical to today (output + files).
- TUI promote path covered by routing through the same core (unit or
  integration, whichever the existing promote tests use).
- fmt/clippy at the 18/6 baseline; suites green.
