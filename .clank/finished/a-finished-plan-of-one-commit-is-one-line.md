# a-finished-plan-of-one-commit-is-one-line
# A finished plan of one commit is one line

## Why

> "if a plan has a single commit and it is finished can you make it
> display as one line in the timeline. No need to show the commit
> message in that case. Just make a hybrid line where it shows the
> time, the flag icon, the commit and the plan name with its usual
> highlight."

Autosquash collapses every finished plan to exactly one commit, so the
common case is now the case the timeline handles worst: an umbrella
header carrying the plan name, and directly beneath it a single commit
row whose subject IS that plan's finish message. Two lines, one fact.

The header has no time and no sha; the commit row has no plan
highlight. Neither line is complete on its own, which is why both are
there.

## The model

**One fact, one line** — but the fact has to come from the fold, not
from the rows.

The umbrella exists to group SEVERAL commits under a plan. With one
commit there is nothing to group, and the two half-rows can be one row:
the commit's time and sha, the `⚑` that already marks a finalize
commit, and the plan name in `Style::Highlight` where the subject would
have gone — because for a finished single-commit plan the subject and
the plan name say the same thing.

The first draft got two things wrong, both found by codex against the
code.

**A header with one commit under it is not a one-commit plan.**
`oneline_items_rows` flushes its chunk at every GitHub event and plain
commit, and each chunk emits its own umbrella — so a two-commit plan
with a GitHub event between them yields a header whose only commit is
the finalize. A windowed read does the same by showing a plan's tail.
Collapsing on what a chunk happens to hold would break the two-commit
promise in both cases.

The fold already knows, exactly and independently of any window:
`FinishedPlan { plan, intro, finalized_at }`. **A finished plan is one
commit iff `intro == finalized_at`.** One comparison, no counting, and
nothing about what is on screen.

**Reviews render ABOVE their commit, not below.** A review happens
after its commit, so newest-first puts it above — and
`entry_overlay_target` binds one to its sha by scanning FORWARD to the
next `Commit` row, stopping at a `Header` as a section break;
`ledger_rows` uses the same relationship in reverse. Moving reviews
under a new hybrid row, as this plan said, would orphan them or bind
them to a later commit.

Which settles the row's shape: not a new kind, but `OnelineRow::Commit`
carrying the plan it also stands for. The forward scan finds it, the
ledger finds it, its commit page is still its own — and the renderer
draws the plan name in the header's highlight instead of the subject.
The plan's own `Header` is suppressed where that happens.

## Deliverables

1. **Eligibility from the fold**: a `FinishedPlan` whose `intro` equals
   its `finalized_at`. Decided before any chunking or windowing, and
   carried into the row — never inferred from what a chunk contains.
2. **`OnelineRow::Commit` gains the plan it stands for.** Explicit
   identity, not a subject string dressed up as a name: the row is
   still a commit to everything that navigates by commits.
3. **The line is: age, `⚑`, short sha, plan name highlighted.** No
   subject — it would repeat the name. The plan's `Header` is
   suppressed for exactly those plans.
4. **Reviews are untouched**, in position and in target. This is a
   display change; the navigation contract is not part of it.
5. **The web ledger collapses identically**, through the same rows —
   there is one producer, `recent_log_rows`, and both surfaces read
   its output, so this holds by construction rather than by two edits
   staying in step.

## Tests

- A finished plan whose intro IS its finalize is one row: age, flag,
  sha, highlighted name, no subject.
- A finished TWO-commit plan is unchanged — header plus two — including
  when a GitHub event splits it across chunks so that one chunk holds
  only its finalize. This is the case the first draft would have got
  wrong.
- A windowed read showing only the tail of a two-commit plan does not
  collapse it: eligibility never consults the window.
- An UNFINISHED single-commit plan is unchanged.
- The ad-hoc bucket (`plan: None`) is unchanged.
- A review above a collapsed row still opens ITS OWN sha — in the TUI's
  overlay target and in the web ledger's page attribution.
- The TUI's rows and the ledger's rows agree, on the same fixture.
- Mutation-check each.

## Out of scope

- The `clank log` CLI renderer, unless it shares the derivation for
  free.
- Anything about how plans are squashed. This is what the timeline
  SHOWS, not what history holds.
