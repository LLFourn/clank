# the-log-splices-git-history-it-does-not-fold-it

## Why

Three complaints about the log, one shape underneath.

**An empty log is still selectable.** The cursor enters a region with
nothing in it, which reads as a broken pane rather than an empty one.

**A repo with commits can show an empty log.** Before clank is adopted
— or in a repo it was never adopted in — every commit is invisible,
even `HEAD`. The tool reports "nothing happened" about a repo with a
million commits.

**Going back is not possible, and could not be cheap today.** The log
shows a window of clank events with no way to page further into
history. Adding one naively is worse than not having it: the clank
state fold would walk commits that contain no clank data at all, so
paging back through a long pre-adoption history would cost work
proportional to that history.

## The shape

The log conflates two different things and folds both.

- **Clank events** — plan intros, commits, reviews, finalizations.
  These come from folding repo state, and folding is the only way to
  get them.
- **Git commits** — a sha, a subject, an author date, and which refs
  point at them. These need no fold at all.

Every commit before clank was adopted is in the SECOND category
exclusively, and there can be an unbounded number of them. Folding
state over a commit that touches nothing under `.clank/` produces
nothing, so that work is not merely expensive — it is guaranteed
empty.

## The invariant

**Cost is bounded by what the reader asked for, never by the
repository's age.**

Paging back N rows costs O(N) git reads and NO additional folding.
Whether the repo has fifty commits or a million before clank appeared
must not change the cost of anything.

## Approach

Splice, do not extend.

The fold keeps its current job — clank events, over the commits that
actually carry clank data — and its existing checkpoint machinery
already bounds that. Adopt a boundary: the earliest commit carrying
clank data. At and after it, rows come from the fold. Before it, rows
are PLAIN git commits read directly, with no fold consulted and no
clank state attributed to them.

A plain row shows what git knows: short sha, subject, and any ref that
points at it — so `HEAD` on a repo clank has never touched is visible
and marked with its branch, which is the second complaint's fix and
falls out of the same change rather than being a special case.

**Paging** reads further back from the boundary in fixed-size batches,
one batch per request. The reader's window governs the work.

Determining the boundary must itself be cheap and must not require
walking pre-adoption history to find it — `state.adopted` already
exists in the fold and the checkpoints already record depth, so the
answer is likely available without a new walk. Establish that before
implementing; if it is not, that lookup is the first thing to build,
because a boundary search that walks the whole history reintroduces
the cost this plan removes.

**The empty case disappears on its own.** A repo with any commit has at
least one row, so "empty log" narrows to "repo with no commits at all",
and the region should refuse selection only in that genuinely empty
case.

## Required tests

- A repo with commits but NO clank data shows its commits, `HEAD`
  among them, carrying its branch.
- Those rows carry no plan attribution — a pre-adoption commit is not
  silently folded into a plan.
- Paging back across the adoption boundary produces continuous rows:
  folded on one side, plain on the other, no gap and no duplicate at
  the seam.
- **Paging back does not fold pre-adoption commits.** Assert it on the
  fold's own counters rather than on wall-clock, so the guarantee is
  structural: N rows of pre-adoption history costs zero folds.
- Cost does not grow with pre-adoption depth — the same request over a
  short and a long pre-adoption history does the same work.
- A repo with no commits at all yields no rows and refuses selection.
- No test spawns zellij or an agent binary.

## Out of scope

- What the log RENDERS for clank events. This changes where rows come
  from, not how a plan row looks.
- Reworking the checkpoint policy. It already bounds folding; this
  plan bounds what is asked of it.
