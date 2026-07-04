# clank-pick
# `clank pick` — copy a stack of plans from another branch into HERE

## What (lloyd)

Copy one or more plans — their commits and plan files — from another branch
or worktree onto the current one. COPY, not move: the source branch is never
touched. The ergonomics of `git-branchless`'s `git move` (name a source,
destination defaults to your current state) with non-destructive semantics.

```
clank pick <plan>... --from <branch|committish>
```

Run in the TARGET worktree (pull model). "Pick the `foo` and `bar` plans
from that branch." The name follows git's own shorthand: in a rebase todo
the copy verb is plain `pick` — and clank picks a whole plan WITH its
context (intro, its commit run, its stated why), so it needs the
"cherry-" qualifier even less than commit-level picking does.

Taxonomy note: `pick` = copy (source untouched, no new state anywhere);
the stash dance (`rename-shelve-to-stash` draft) = move. Two orthogonal
verbs, not one command with a mode flag that flips safety profiles.

## REVIEWERS: weigh the CLI semantics especially hard (lloyd)

The command's comprehensibility is the primary design surface of this plan
— more than the mechanics, which are mostly assembled from existing
machinery. Take special consideration of:

- Does `pick` read as COPY to a git user, or does cherry-pick's
  sha-changing heritage make it ambiguous with move? Is `--from` the right
  preposition? Is pull-model-only comprehensible?
- Is plan-level granularity obvious from the invocation shape, or will
  users expect commit-level picking?
- PROPOSE ALTERNATIVES if you think there is a better verb or shape —
  seriously considered candidates so far: `pick` (git rebase-todo
  shorthand), `import <plan> --from` (directional, vaguer mechanism),
  `duplicate` (jj's copy verb, connotes in-place), `transplant` (hg
  heritage, but connotes MOVE — reserved for a possible future move verb),
  `graft` (hg's copy verb, but git's grafts/replace plumbing gives it
  baggage), `copy` (plain, collides with file-op intuition). Name a better
  one if you see it, with the reasoning.

After the intro clears the gate the master will BLOCK and present the
option space to lloyd for the final call — so verdicts here should argue
the naming/shape question explicitly, not wave it through.

## DECIDED (lloyd, via block): `clank pick` — option 1

Blocked after gate-continue per the plan; lloyd chose `pick <plan>...
--from <branch>` (codex had endorsed the same shape at intro review).
Implementation may proceed.

## Mechanics

1. **Fold the source tip, not HEAD.** The one new primitive is
   fold-at-a-named-tip: resolve `--from` to a commit, fold its first-parent
   history (precedent: `re_fold_finished_plan_natives` already cold-folds to
   an arbitrary commit; `git_io::first_parent_commits_to` exists), yielding
   that branch's plans map + finished list.
2. **Locate each named plan on the source**: active → its `native_shas`
   from the fold's per-plan timeline; finished → `FinishedPlan { intro,
   finalized_at }` + the same natives re-fold the rewrite preview uses.
   A finished autosquashed plan is the degenerate happy case: ONE
   self-contained portable commit (an underrated payoff of the autosquash
   default).
3. **Order the stack by source position** (intro order, never argument
   order) and **cherry-pick** the combined commit list bottom-up onto the
   current HEAD. Cherry-pick (diff-based 3-way) — NOT the engine's
   `replay_commit`: tree-preserving replay is only sound onto the same
   base; across bases it would clobber the target's content. Reuse the
   cherry-pick plumbing unshelve's pop path uses (subprocess inside
   git_plumbing, with its "why not gix" note).
4. **The intro commit carries `plans/<stem>.md`**, so a picked active plan
   enters the target's review cycle immediately: gate unreviewed, reviews
   RESET by design (new shas, new base, new context — same rule as
   unshelve; a review of the work on the other branch says nothing about
   it replayed here). No feedback migration, deliberately.
5. **Conflicts** stop mid-pick in git's normal cherry-pick state for
   manual resolution; the error names `git cherry-pick --abort` as the
   bail-out. Nothing on the source needs rolling back — it was never
   touched — so no protective refs, no new state files, no records.

## Refusals / warnings

- Stem already ACTIVE or FINISHED on the target → refuse (same collision
  rule as promote). Suggest finishing/purging the local one first.
- Foreign commits interleaved WITHIN a picked plan's source range →
  refuse, naming the offenders (same rule + wording as the squash guard).
- Dirty target worktree → refuse before the first pick (git would anyway;
  fail early with a clank-voiced message).
- WARN (not refuse) when a picked plan's commits sit ABOVE unpicked plans
  on the source — textual dependence on what's left behind is the likely
  conflict source. Name the plans below it.

## CLI details

- `--from` is REQUIRED for v1 (no clever defaulting to "the other
  worktree's branch"; explicit beats guessing).
- `--dry` prints the resolved plan order and each plan's commit list
  (sha + subject) without picking — computed from the same fold the live
  run uses (the one-computation rule; no hand-rolled preview text).
- Multiple plans allowed; a single plan is the common case.

## Tests (in-process; git fixtures OK)

- Pick an active plan from a side branch: plan file lands in `plans/`,
  commits replayed in order, target fold shows the plan active +
  unreviewed; source branch byte-identical before/after.
- Pick a finished autosquashed plan: one commit, `finished/<stem>.md`
  present, fold marks it finished on the target.
- Two plans picked in reverse argument order still replay in source
  order.
- Refusals: existing stem on target; interleaved foreign on source
  (named); dirty target.
- Conflict: a pick that collides leaves git's cherry-pick state and a
  clear error; `--abort` guidance present; source untouched.
- `--dry` is a strict no-op (HEAD, refs) and lists the same commits the
  live run then picks.

## Open questions

- OQ1: pick a plan that's QUEUED (not committed) on the source? Lean: out
  of scope — queue items are plain files; copying one is `cp` (or a future
  `queue pick`). This command is about committed plan stacks.
- OQ2: `--onto <branch>` push-model variant? Lean: no — pull-model only;
  the target must be a checked-out worktree for conflict resolution to
  have somewhere to live.
- OQ3: partial pick (a subset of a plan's commits)? No — plans are the
  unit; commit-level surgery is git's job.
