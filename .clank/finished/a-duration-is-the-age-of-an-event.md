# a-duration-is-the-age-of-an-event

> Whenever an agent is working in `clank status --tui` we always need
> some kind of time estimate for how long it's been going. It doesn't
> need to be updated to the second, but to the minute would be good.
> Same for the commits in the current plan: it would be good to know
> how long ago roughly — "2d", "3h", "20m" — the commits and the
> feedback were made.
>
> It only needs the elapsed time for the most recent handful of events
> updated at most every 1m. I guess it could be for all visible events
> since there's not that many and subtracting times is not that much
> work. — lloyd

## Today

Nothing on the panel or in the log says WHEN. An agent row spins with
an italic verb — `⠙ reviewing…` — and reads exactly the same at ten
seconds and at three hours, which is the whole difference between "it
is fine" and "go look at it". The log's commit and review rows carry a
marker, a sha, a subject and an author, and no time at all: `clank
html` prints each event's ISO timestamp (`html.rs::event_ts`), but the
same rows in the TUI drop it, and a review has never carried a time
anywhere.

The one clock on screen is the attendance marker's `⌛ cargo test ·
3m/5m`, and it exists only because `clank run` RECORDS when the work
started.

## The model

> Every duration on screen is the age of a recorded event. The TUI
> times nothing itself.

The tempting shortcut — remember the first frame an agent was seen
spinning — is rejected outright: it resets on every restart, and it
would report five seconds for work that had been going for an hour,
which is precisely the case this plan exists to catch. So each
duration on screen resolves to a timestamp that already exists on
disk:

- **a commit** — its author time. The fold carries it on every
  `LogEvent` (`ts`) and on `CommitMeta::author_ts` for the
  pre-adoption rows.
- **a review** — its feedback file's mtime. Feedback is gitignored and
  machine-local, written once by `clank feedback write` (tmp +
  rename), so the file's write time IS when the review was made.
- **a PR review round** — `pr-reviews/<n>/pr.json`'s mtime for when
  the round opened (only `start` and `propose` write it, both
  master-only), and `reviews/<label>.md`'s mtime for a verdict.

`LogEvent::ts()` moves to core as a method (html's private `event_ts`
becomes its first caller), so "the time of an event" is asked for in
one way.

### One formatter, one resolution, recomputed on every paint

`stop_hook::short_duration` (`s`/`m`/`h`/`d`, one unit) and
`events::age` (the same ladder over epoch seconds) are the same
function written twice. They move to `crate::age` — one home, one
ladder, both old call sites calling it, the way `shell_quote` was
hoisted. The attendance marker keeps that ladder: `45s/5m` is a
BUDGET, and a five-minute bound is read in seconds.

Every clock this plan adds is minute-resolution: `age::coarse`, which
is the same ladder with `now` in place of any seconds value. Agent
rows and log rows both use it, so there is one shape of duration on
screen to learn and none of it needs a fast tick to stay honest.

Which makes the cost question answer itself. Each duration is one
subtraction and a format, done for the rows being drawn, in the render
pass that already runs — no cache, no invalidation, no per-row timer,
and no new wake-up. The loop's idle repaint is already 60s (`base`),
which is exactly the resolution shown, so the elapsed on screen is
never more than one tick behind and the pane sleeps between paints
exactly as it does today.

The relation is arithmetic and asserted as such: `coarse`'s smallest
non-`now` unit is not smaller than the loop's idle tick.

### Where the times are read

Two readers already open every feedback file that matters, and each
gains one `stat` of the file it is already reading:
`FsPlanStateLookup::reviews_for` (what the gate folds) and
`collect_reviews` (what the log renders). Both resolve paths through
the canonical parser — full-sha and short-sha forms — so neither
learns a new way to find a review, only when it was written.

A rewrite must not disturb any of this. `rewire::copy_file` copies a
feedback file to the rewritten sha with `std::fs::copy`, which stamps
the destination with the copy time — so today an amend would make
every review on that commit read as "written just now". The copy
carries the source's mtime over (`File::set_modified`): a review was
written when it was written, and moving it to a new sha does not
change that.

### What an agent's elapsed measures

> The spinner says the agent owes something. The elapsed says how long
> it has owed it — the age of the OLDEST obligation it currently
> carries.

Monotone until the agent acts, which is what makes it worth glancing
at. The obligations are not re-derived: `work_projection(snap)` is
already in `derive.rs`, and `work_for(label, role)` — the same single
source that decided the row spins at all — enumerates them.

### The review chain has stages, and each has a boundary

A first cut dated an obligation from a `Handover { opened_at,
last_verdict_at }`: the commit for a commit-tier reviewer, the newest
verdict for everyone else. Codex struck it, and the reason is
structural rather than a missing field. "The newest verdict by anyone"
is not a boundary of anything: with two second-tier reviewers pending,
the first one submitting would move the second one's clock forward
though nothing had been handed to them, and the "monotone until the
agent acts" property this whole surface rests on would be false on the
very cycle it matters. Adding a third field would fix that instance;
naming what is actually being asked for makes the class unwritable.

The gate is a CHAIN OF STAGES, and each stage closes exactly once, at
a boundary that is the max over the verdicts REQUIRED to close it —
never over "whoever wrote something":

    Handover {
        /// The work appeared: the commit's author time, or the PR
        /// round's open time.
        opened: i64,
        /// The commit tier closed — max over the REQUIRED commit-tier
        /// verdicts. None while any of them is pending.
        commit_tier_closed: Option<i64>,
        /// The tier the milestone activated closed — max over ITS
        /// required verdicts. None while any is pending, and None
        /// when no second tier is active.
        second_tier_closed: Option<i64>,
    }

An agent's clock starts at the boundary of the stage that summoned it,
and the tier it answers on is the roster's (`RosterRole`: commit →
`Commit`; plan / final / gate → `Second`; master → `Master`):

    Commit => opened
    Second => commit_tier_closed.unwrap_or(opened)
    Master => second_tier_closed.or(commit_tier_closed).unwrap_or(opened)

Every case now falls out of one rule, and the peer reset cannot be
expressed: a boundary is a max over a fixed required set, so a verdict
from outside that set — a second-tier peer, an unsummoned reviewer, a
stale author whose feedback dir still exists — moves nothing. Master's
handback is the same rule read to its end: when the commit tier
requested changes, master was handed the plan back when THAT tier
closed, and a gate reviewer writing an unsummoned verdict afterwards
does not restart master's clock either. The fallbacks are the empty
tiers (a final-only repo has no commit tier; a routine WIP commit
activates no second tier).

### It is computed where the stages are decided

Which reviewers are required, and which second tier a milestone
activates, is `compute_gate`'s knowledge — `tier_coverage` and
`active_second_tier`, in core. Recomputing any of it in the TUI to
attach a timestamp would be a second copy of the gate, and it would
drift. So the boundaries are computed in core beside the gate, from
the same helpers, and ride the work states the gate already produces:

- `ReviewEntry` gains `at: Option<i64>` — the CLI's two feedback
  readers (`FsPlanStateLookup::reviews_for` for the gate,
  `collect_reviews` for the log) each stat the file they already open.
- `PrReviewInput` gains `opened_at` (the round's `pr.json` mtime),
  and its current-round verdicts carry their own.
- `stage_closures(reviews, tiers, latest_touched_plan) -> Handover`
  sits next to `compute_gate` and shares its helpers; `PlanWorkState`,
  `AdHocWorkState` and `PrReviewWorkState` each carry the result.
- `InProgress::{PendingReview, MasterWorking}` gain
  `since: Option<i64>`, computed in `in_progress_rows` — the one place
  that already decides who is working and why — by mapping each
  `WaitItem` to its work state's `Handover` and taking the min over
  the agent's obligations. Items that are not obligations (`Finished`,
  `Idle`) map to no clock; a missing clock draws no elapsed and leaves
  the spinner alone.

### On screen

One time column, right-aligned, dim, down the whole pane: an agent's
elapsed and a row's age line up in the same cells, so "when" is always
read in one place.

- `text::gap_fill(left, right, cols)` composes it — the rule `bar()`
  already uses: the right segment is kept only when at least two
  columns of gap remain, and is dropped WHOLE otherwise. It returns
  spans, so `row_line` truncates and the selection band paints exactly
  as they do now.
- **Agent rows**: the elapsed rides with the spinner, so an idle
  agent's row is unchanged. An ATTENDING agent is unchanged too — it
  is blocked, not working, and its `⌛ … · 3m/5m` marker is its own
  clock. No row ever carries two.
- **Log rows**: commit, plain-commit, review and github rows carry
  their age. Umbrella headers and notices do not — they are not
  events.
- The bar keeps its lamp text: the agent row is where the verb lives,
  and one clock per fact.

## Tests

- `crate::age`: the ladder's boundaries (59s→`59s`, 60s→`1m`,
  3599s→`59m`, 3600s→`1h`, 86399s→`23h`, 86400s→`1d`); a negative
  duration (clock skew, a commit from the future) reads as zero, never
  as a wrapped number. `coarse` never yields a seconds string at any
  input and reads `now` below a minute. Both old call sites keep their
  output, the attendance marker's seconds included.
- The arithmetic behind the resolution rule: `coarse`'s smallest
  non-`now` unit is not smaller than the loop's idle tick.
- `collect_reviews` puts the file's mtime on the `Review`; a file it
  cannot stat yields `None` and still renders.
- `rewire`: a feedback file with an mtime an hour old keeps that mtime
  through `migrate_feedback_pairs`, and its content still arrives.
- `stage_closures` (core, pure, next to the gate it shares helpers
  with):
  - `commit_tier_closed` is the LAST required commit-tier verdict, and
    an unsummoned second-tier verdict written after it moves nothing;
  - a stale author (feedback from someone off the tier) never dates a
    boundary, the same way it never gates one;
  - `second_tier_closed` is the last verdict of the tier the milestone
    ACTIVATED — a plan-tier verdict does not close a finish milestone,
    and vice versa;
  - a stage with anyone still pending is `None`, and empty tiers give
    `None` (not a vacuous "closed now").
- `Handover::since` per tier, and the invariant codex asked for
  by name: with TWO second-tier reviewers pending, the first one
  submitting leaves the other's `since` exactly where it was — a peer
  is not a handover. Its master analogue: the commit tier requested
  changes at T1, a gate reviewer wrote an unsummoned verdict at T2,
  master's clock still reads from T1. The same pair for a PR round.
- `in_progress_rows`:
  - a commit-tier reviewer dates from the commit even when a peer
    verdicted afterwards; a second-tier reviewer dates from the commit
    tier's close, NOT the commit — the two differ in the fixture, so
    collapsing the tiers to one rule fails;
  - master revising dates from the tier close that handed it back;
    master on an unreviewed commit dates from the commit;
  - an ad-hoc revision dates from the ad-hoc commit;
  - an agent owing two things shows the OLDER one;
  - no clock for the obligation → `since: None`, and the row still
    spins.
- The pane's own cadence is untouched: with a clock on screen and
  nothing spinning, the loop still sleeps its idle interval — no timer
  is armed for the elapsed and no refresh is requested by it.
- Render: an active agent row carries its elapsed in the right column;
  an idle one carries nothing; an attending agent shows the marker's
  clock and no second one (the existing "blocked is not working"
  assertions still hold).
- Render: a commit row and a review row carry their ages, aligned in
  the same column as the agent rows'; a header carries none; a pane
  too narrow drops the age WHOLE and the subject keeps every column it
  has today.

Mutation-checked with production-only edits: the tier distinction
collapsed to one rule; a boundary widened from its required set to
every verdict on the commit; `min` swapped for `max` over obligations;
the mtime preservation removed; `gap_fill`'s drop rule turned into a
truncation.

## Out of scope

- `clank log --oneline`, `clank status` (non-TUI) and the html export:
  html already prints absolute ISO times, and the ask is about the
  live pane.
- Stash and queue rows (age of the stash, age of the queued idea).
- The agent detail page and the bar.
- Absolute times anywhere, and any second unit (`2d 3h`) — one unit is
  what the ask named and what the existing formatter gives.
