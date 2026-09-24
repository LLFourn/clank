# finish-works-on-the-review-the-loop-makes

`clank pr-review finish` cannot set the body of a review the loop
produced, so it fails on every one.

> "GitHub's error is: `Could not edit a review with a missing body.`
> … So `finish` cannot succeed on any review the loop produces. The
> existing test `the_body_endpoint_updates_without_submitting` only
> asserts the argv shape and never exercises this." — claude
> (/Users/llfourn/src/fswt/pr-603), on frostsnap #603, pending review
> 5299225119

## Why

`finish` hands the converged review over as a draft by PUTting a body
onto the pending review (`gh::update_body_argv`, `pr_review.rs:890`).
GitHub's REST update-a-review endpoint refuses that with 422 when the
review's current body is EMPTY.

The loop's pending review always has an empty body. clank's own
`create_summary_argv` sends one (`pr_review.rs:1086`), but that is not
the path the loop takes: master's first call posts inline comments
only, so the review GitHub creates has no body for the later PUT to
edit.

The knock-on is the part that makes this urgent. `finish` exits 1
without recording anything, so the `pr-review: master finish #<n>`
wake item re-fires on every Stop, forever. An agent that cannot
finish also cannot stop being asked to.

**Not independently reproduced here.** The 422 and its message come
from the sender, who hit it on a live PR and reproduced it with a
trivial body. What IS confirmed in this repo: `create_summary_argv`
sends a body and the loop's creating call does not, and
`the_body_endpoint_updates_without_submitting` (`pr_review.rs:2037`)
asserts only the argv vector — it never runs a request, so no test
could have caught this. Establishing the endpoint's behaviour against
a real empty-bodied pending review is the plan's first step, because
the fix depends on which of the two causes it is.

## The build

**Find out what the endpoint actually refuses**, before choosing a
fix. Either GitHub rejects editing an empty body (the sender's
reading), or it rejects something else about a review created without
one. A throwaway PR on a repo of ours, a pending review created the
way the loop creates it, then the PUT — torn down afterwards, as the
earlier GitHub probes in this repo were.

**Then one of two fixes, chosen by that answer:**

- The pending review is created with a non-empty body, so the later
  PUT edits an existing one. Cheapest if the refusal is really about
  emptiness — but it only works where clank creates the review, and
  the loop's review is created by the skill, so this may not reach it.
- `finish` sets the body over GraphQL
  (`updatePullRequestReview(input:{pullRequestReviewId, body})`).
  Precedent: the loop already edits pending comments this way, because
  the REST PATCH 404s on a pending comment. If REST is simply the
  wrong API for a pending review, this is the same lesson twice.

**A test that would have caught it.** The existing one asserts a
vector of strings; the bug is in what GitHub does with it. Whatever
covers this has to exercise the call against a review shaped like the
loop's, not its argv.

## Tests

- `finish` succeeds against a pending review created the way the loop
  creates it — inline comments, no body.
- The body it sets is readable afterwards, and the review is still
  PENDING: `finish` must not publish.
- A test that fails if the call reverts to the shape that 422s.

## Out of scope

- The skill text. `~/.claude/skills/clank-pr-review/SKILL.md` names no
  `finish` and still tells master to `clank pr-review submit`, which
  is not a command any more (the verbs are start/note/propose/abort/
  finish/status). That is a real bug and it is why the sender blocked
  instead of finishing, but the file is outside this repo and is the
  user's global config — raised separately rather than edited here.
- #603 itself. Its pending review is untouched and stays that way.
