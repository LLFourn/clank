# the-plan-name-sits-under-its-commits
# The plan name sits under its commits

## Why

> "in `clank tui` and clank log (if appropriate) can you put the plan
> name line beneath the plan commits rather than above them."

## The model

**The timeline runs newest-first, so a plan's beginning belongs at its
bottom.** The umbrella header prints before its run today, which puts
the plan's name next to its NEWEST commit — the finish — and leaves the
intro, where the plan actually began, orphaned at the far end of the
run.

The timeline already reads this way everywhere else: a review renders
ABOVE its commit because a review happens after it (`log.rs` says so
in as many words). By the same clock the name of a plan belongs after
its commits, adjacent to the intro that opened it.

`oneline_rows` walks `umbrella_sections` and pushes the header before
the run. Moving it after is one move — but it is not only a move,
because two things bind to the header's POSITION:

- `entry_overlay_target` resolves a review's commit by scanning
  FORWARD to the next `Commit`, and stops at a `Header` as a section
  break. A header that now follows its commits is a break in a
  different place, and a review at the top of a run must still find
  its own commit rather than the previous plan's.
- `ledger_rows` walks in reverse and attributes pages by the same
  relationship.

Both have to be re-read against the new order, not assumed to survive
it.

Swept before promoting: those are the ONLY two production consumers
that depend on where a header sits. Every other `OnelineRow::Header`
site styles it, skips its age gutter, or prints its name, and the rest
are test fixtures. The scroll sequence is pure history with no
placeholders spliced against a header.

Both look likely to survive, and the reason is worth stating so the
work is a check rather than a rewrite: `oneline_rows` pushes a
commit's reviews immediately before that commit, so within a run a
review always has its own commit adjacent — forward for
`entry_overlay_target`, backward for `ledger_rows`. The header break
in each is then the CHUNK-BOUNDARY case (a run split so a trailing
review has no commit after it), not the common path. If that holds,
deliverable 3 is a test proving it rather than a change — say so
plainly instead of inventing a correction.

## Deliverables

1. **The header follows its run** in `oneline_rows`, for the TUI and
   the web ledger, which share that one producer.
2. **`clank log --oneline` follows**, since it renders the same rows —
   "if appropriate" is yes: the same reason applies, and two orders
   would be worse than either.
3. **The section-break scan is checked against the new order** — and
   it needed NO correction, which is the honest outcome rather than an
   invented change. `oneline_rows` pushes a commit's reviews
   immediately before that commit, so within a run a review always has
   its own commit adjacent: forward for `entry_overlay_target`,
   backward for `ledger_rows`. The header break in each is the
   chunk-boundary case, not the common path. What the work adds is the
   PROOF: both consumers are now driven by producer-generated rows,
   because the pre-existing fixtures hand-place the name on top and so
   could not have caught a break here.
4. **The single-commit collapse still holds**: a finished plan of one
   commit has no header at all, so it is unaffected — but the tests
   that pin it must keep passing, which is the cheapest proof the two
   changes do not interact.

## Tests

- A plan of two commits renders its commits then its name, in the
  TUI's rows and the ledger's.
- A review above the newest commit of a run still opens THAT commit's
  sha, not the neighbouring plan's.
- Two plans in sequence keep their commits attributed to the right
  name — the case a misplaced section break breaks.
- The ad-hoc bucket, which has no header, is unchanged.
- A finished single-commit plan still renders as one line.
- `clank log --oneline`'s shape is pinned by its existing round-trip
  test; update it deliberately rather than letting it drift.
- Mutation-check: putting the header back above must fail a named
  test.

## Out of scope

- What the header CONTAINS, and the highlight it wears.
- Anything about ordering within a run, which stays newest-first.
