# the-review-session-watches-its-pr

While a PR review is open, master knows what happened on that PR —
including what the operator did to the draft.

> "in PR review mode the master agent should probably wake for every
> event on that PR, comments being edited or new comments etc. It
> shouldn't follow up with anything just be aware." — lloyd

This is the third of three asks in
`the-review-is-handed-over-not-submitted`. That plan built the first
two (the draft is handed over, the thumbs come off) and deferred this
one, because it rests on four things that did not exist. It is queued
separately so the ask outlives the plan that deferred it.

## Why

A review in progress is a conversation. The operator edits one of
master's comments, deletes another, or replies in the GitHub UI while
the team is still iterating — and master integrates the next round
against a draft that has moved under it. Worse, the handoff now ends
with the human editing that draft directly, so the case is no longer
hypothetical: it is the intended workflow.

The wake is AWARENESS only. Master re-reads the draft before its next
round; it does not reply, react, or follow up on the operator's edit.
An agent that answers the human's edit is a second reviewer nobody
asked for.

## The three things this has to establish first

Each is a real gap, verified against the code rather than assumed.
The fourth — a PR scope for the comment kinds — has since landed:
`a-watch-sees-only-its-prs` (02b85fe) made `GithubSource.prs` scope
the kinds that can be scoped, at every door an event reaches an agent
through. Not every kind: `is_pr_scoped` deliberately excludes
`PrOpened`, since a PR that does not exist yet cannot appear in a list
written before it (agent_config.rs:175). So this watch does not
inherit a scope by listing kinds — it must confine every kind it
subscribes to, and must not carry `PrOpened` at all.

- ~~**The operator's own edits must count.**~~ Settled, by ruling the
  filter out. The team shares one `gh` account, so "the agents" versus
  "the person" cannot be expressed by a login — and clank does not
  perform the writes either, so it holds no provenance to key on. Two
  attempts to stand something else in its place both failed the same
  way: an interval (a round, a park) declares whatever the OPERATOR
  did inside it to be the agents' work and erases it, which is the one
  thing this watch exists to catch (codex ce892ea).

  So nothing is filtered. Every change to the draft presents,
  including master's own. The cost is one spurious wake per round —
  master parks after writing, is told about its own comments once, and
  the horizon is quiet thereafter — and that is the correct trade for
  an awareness item: a wake master can ignore costs a cycle, a wake
  that never comes costs the operator's edit. Suppression can only be
  reconsidered by a plan that routes the agents' writes through clank,
  which would give it real provenance.
- **An identity that distinguishes successive edits.** A comment's
  action key is its id alone, so a comment edited twice is one action
  and the second edit dedups away. An edit needs an identity that
  changes when the body does.
- ~~**Whether a PENDING draft's comments are observable at all.**~~
  Answered below. The
  draft is private to its author. That webhooks deliver published
  review comments says nothing about unsubmitted ones; it has to be
  checked against a real PR, not inferred. If they are not delivered,
  a bounded authenticated re-read of the draft is the honest
  substitute — and it is the same shape as the CI reader, which reads
  state rather than receiving events. A re-read brings its own rule:
  diff only COMPLETE successful snapshots, or a failed page reads as
  a deletion; and remember observed revisions rather than a forever-set
  of body hashes, or an A → B → A edit never presents again. Polling
  reports what it observed, not every intermediate edit between two
  samples, and the findings must say so.

The first milestone answers the last question, because it decides
whether the rest is a watch or a re-read.

## Findings: it is a re-read

Measured against a real PR (LLFourn/clank#1, opened and deleted for
this, review 5297636149, comment 4088032661), with
`gh webhook forward` running throughout and a published
`issue_comment` as the control that proves it was working:

| on a PENDING review comment | webhook | `/events` feed | authenticated re-read |
| --- | --- | --- | --- |
| create | nothing | an event — then retracted | sees it |
| edit | nothing | nothing | sees it |
| delete | nothing | RETRACTS the create event | sees it |

The control `issue_comment` was delivered in under 20s while every
pending operation delivered nothing, so "no webhook" is a measurement
and not a timeout.

**The feed is disqualified, not merely incomplete.** It carried the
creation (`PullRequestReviewCommentEvent`, action `created`, full
body, publicly — an unauthenticated read saw it too) and then
UNPUBLISHED it when the draft comment was deleted: id 15625463676 is
absent from the whole paginated feed afterwards. The cursor is a
BOUNDED seen-id set that evicts past `SEEN_CAP` (`feed.rs`), so the
problem is not that a retracted id is remembered too long. It is that
the feed asserts something happened and then withdraws the assertion,
while the cursor's only question is "have I already emitted this id" —
there is no shape in which a retraction reaches a watch that already
woke on it. The feed must not be the source of truth for a draft.

**The revision is the observed transition, not a field.** Editing
A → B → A advanced `updated_at` each time (22:43:56 → 22:45:55 →
22:46:36) while the body returned to its original text, so a set of
body hashes would silently swallow the second edit. But `updated_at`
cannot be the identity either: it is a whole-second timestamp with no
documented guarantee that successive mutations get distinct values,
and the probe's edits were 119s and 41s apart — it never tested two
edits inside one second. A reader keyed on `(id, updated_at)` would
discard a changed body it actually observed.

So the durable state is the last observed comment STATE, and a
revision is assigned by this watch to each transition IT detects: a
changed body is a transition even when `updated_at` is equal, and a
changed `updated_at` is corroborating evidence (including with an
unchanged body) rather than the key. GraphQL's `lastEditedAt` stayed
`null` across both edits and is no help at all.

**The write surface is split; the read surface is not.** `PATCH
/repos/{o}/{r}/pulls/comments/{id}` 404s on a pending comment, so the
operator's UI edit goes through GraphQL
`updatePullRequestReviewComment`; `DELETE` on that same REST path
works. This matters only for anything that WRITES a draft comment —
it is not why the edit is unobservable. The read,
`GET /repos/{o}/{r}/pulls/{n}/reviews/{id}/comments`, shows all three
transitions on its own. Snapshots are needed because no edit or
deletion is DELIVERED, not because the edit cannot be read.

**The limit this accepts.** Polling reports what it observed between
two samples, not every intermediate edit: three edits between reads
present as one change. That is the honest ceiling of a re-read and
the plan does not pretend otherwise.

This closes the fourth question. The build below is a re-read.

## The build

**The watch belongs to the review's lifecycle.** It begins at
`clank pr-review start`, ends at `finish` or `abort`, and belongs to
master alone — reviewers are woken by the round, not by the PR. It is
derived from the active review rather than configured, so no operator
watch is touched and nothing outlives the review that owns it.

**Awareness is a shape, not a wording.** The item says what changed
and on which comment, and the master skill says plainly that the
reaction is to re-read before the next round and nothing else. It is
its own kind with its own ack path, not another actionable github
event: "be aware" must not land in the generic reply/react workflow,
and must not re-wake forever because nothing ever answers it.

**The watch outlives the gaps between waits.** `wait.rs` spawns
external sources only after the initial work projection finds
nothing, and cancels them when it returns — so a source that
baselines afresh on each park is blind exactly while master is
integrating a round. The observation needs a durable review-scoped
baseline and catch-up, the same shape the WAL already gives a github
source. It stops on a SUCCESSFUL `finish`/`abort`, including from an
already parked wait; a failed finish keeps the review, so it keeps
the watch.

**Nothing is attributed, because nothing can be.** The skill writes
its comments through raw `gh`, outside clank's lifecycle API, so no
comment id, login or retained marker says who performed the latest
edit, and clank never sees the write. Every change presents. The one
suppression that would be sound — a write clank performed itself —
needs a write path clank owns, and that is another plan.

## Tests

- An edit and a deletion each present, and a second edit of the same
  comment presents again rather than deduping away.
- The operator's own edit presents. So does master's own write, once:
  the horizon is never re-baselined by a round.
- An operator edit made while master was awake and integrating — no
  wait armed, no round boundary crossed — still presents when master
  next parks.
- A failed snapshot page does not present as a deletion, and an
  A → B → A edit presents both times.
- Two complete snapshots carrying the same comment id and the same
  `updated_at` but different bodies present as a change. Deterministic
  fixtures, not a live PR: the probe could not produce two edits
  inside one second.
- The watch ends with `finish`/`abort`, and an operator-configured
  watch on the same repo is unaffected.

## Out of scope

- **The PUBLISHED half of the PR** — the human's own comments, pushes,
  the merge, CI. It looked like a derived `GithubSource` away, and it
  is not (codex ee2c17f). An ordinary watch has no review instance, no
  review-scoped cancellation and an agent-wide WAL that outlives the
  review; its `source_key` collides with an operator watch carrying
  the same filters, so arming it deletes that operator's prompt
  sidecar; and it presents as an actionable event, which is the ack
  workflow this plan exists to stay out of. Underneath all that, the
  feed classifiers admit only CREATED comments — an edited or deleted
  published comment returns `None` from both of them — so the very
  thing the ask names ("comments being edited") needs a successive-edit
  identity in the shared action-key model, which every github watch
  uses. That is its own plan:
  `the-published-half-of-a-reviewed-pr`.
- Any follow-up action. The wake is awareness; replying to the
  operator's edit is what this must not do.
- Changing what `finish` does.
