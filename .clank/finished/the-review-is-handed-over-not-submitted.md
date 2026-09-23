# the-review-is-handed-over-not-submitted

The team's review ends as a clean draft in your hands, not as a review
posted in your name.

> "it is opinionated in not submitting the review. It just makes all
> the comments for you to read and edit through the github interface.
> When the reviwers have marked finished the review is finished
> without submitting the review to github. … when the review is
> finished all thumbs up stuff should be removed from the review
> comments. … in PR review mode the master agent should probably wake
> for every event on that PR, comments being edited or new comments
> etc. It shouldn't follow up with anything just be aware." — lloyd

## Why

`clank pr-review` already does the right thing structurally: the
review scratch IS a GitHub pending review, which only its author can
see, so the team drafts in the open without publishing anything. Then
`submit --event <approve|request-changes|comment>` publishes it, and
that last step is the one the operator did not ask for. A review is a
judgment with your name on it. An agent team can assemble the
substance — where to look, what is wrong, what it would take — but
whether that goes out as APPROVE or REQUEST_CHANGES, and in what
words, is not theirs to decide, and a published review cannot be
taken back.

The draft is also the better handover: GitHub's own interface for
reading, editing and deleting pending comments is right there, and it
is better than anything clank would build.

What the draft must not contain is the machinery that produced it. The
loop's reviewers approve a comment silently with a 👍 reaction, and
those reactions are scaffolding — they mean "this reviewer agreed with
master's draft", not anything about the code. Handing over a draft
covered in the team's own thumbs is handing over the scaffolding with
the building.

## The build

**Converged is finished.** The terminal verb stops publishing: when
every reviewer's verdict is FINISHED for the current round and only
master's top-level comments remain, the review is done. `--event`
goes with it — nothing chooses a GitHub review outcome any more,
because clank no longer states one. The command says where the draft
is and what to do with it (open the PR, read, edit, submit or
discard), and the local scratch is retired the way a finished plan is.

**Finishing cleans the draft.** Every 👍 reaction the loop put on a
comment is removed, and any reviewer reply that survived integration
is surfaced rather than left buried — a draft handed over should read
as master's comments and nothing else. The sweep is part of finishing,
not a separate verb to remember, and it is idempotent: running it on
an already-clean draft changes nothing.

**Master watches the PR while the review is open — once it can.**

A review in progress is a conversation: the operator edits a comment,
deletes one, or replies in the UI while the team is still working, and
master should know rather than integrate against a draft that has
moved. The items would be AWARENESS — master re-reads the draft before
its next round and does not reply, react or follow up.

Four things have to exist first, and none of them do. Naming them is
this plan's contribution to the watch; building them is not, because a
watch that silently cannot see what it promises is worse than no
watch:

- **A PR scope for the comment kinds.** `prs` scopes CI only. Without
  it a review watch wakes on every PR in the repo
  (`a-watch-sees-only-its-prs`, queued).
- **The operator's own edits must count.** The team shares one gh
  account, so `include_own_actions: false` — the default, and
  load-bearing against self-wake loops — filters out exactly the
  human's UI edits this is for.
- **An identity that distinguishes successive edits.** Comment action
  keys are the comment id alone, so a comment edited twice is one
  action and the second edit is deduped away.
- **Whether a PENDING draft's comments are observable at all.** The
  draft is private to its author; that webhooks cover published
  comments says nothing about this. It has to be checked, not assumed
  — and if they are not delivered, a bounded authenticated re-read is
  the honest substitute.

When those land, the watch is derived from the review's lifecycle: it
begins at `start`, ends at `finish` or `abort`, belongs to master
alone, and leaves any watches the operator configured untouched.

That is queued as `the-review-session-watches-its-pr`, so the ask
outlives this plan rather than surviving only as a paragraph in
`.clank/finished/` (ruthless b8d512c).

## Open questions

- **Where master's summary goes.** Today it is `master.md`'s body,
  passed at submit. Nothing sets it afterwards, so the choice is: put
  it on the pending review when the review is created (the reviews API
  takes a body with no event), post it as an ordinary PR comment, or
  leave it in the file for the operator to paste. The first keeps it
  in the draft where the rest of the review is; decide in the intro.
- **Whether `abort` and `finish` stay distinct.** Both end the
  session; one discards the draft, one keeps it. They should read as
  different intentions, not as two ways to stop.

## Tests

- Converging a review leaves the pending review UNSUBMITTED: no
  review is published, and the draft's comments are still there.
- Finishing removes every 👍 the loop added, and running it twice
  changes nothing the second time.
- A finished review's local scratch is retired, and a second finish is
  refused rather than half-run.
- (The PR watch's tests belong with the PR watch; see the build.)

## Out of scope

- The comment substance and the round loop, which are unchanged.
- Submitting on the operator's behalf under any flag. The point is
  that clank does not have that button.
- The awareness watch itself. Its four prerequisites are named above
  and at least one is a queued plan of its own; carrying it here would
  mean shipping a watch that cannot see edits, cannot see the
  operator, and wakes on every PR in the repo.
