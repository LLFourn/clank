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

## What the probe established

Reproduced on a throwaway PR (LLFourn/clank#3, since closed, branch
deleted both sides, review deleted). A pending review created the way
the loop creates it — inline comments, no body — then edited:

    REST    PUT .../pulls/3/reviews/<id>  -f body=…
            422 {"errors":["Could not edit a review with a missing body."]}

    GraphQL updatePullRequestReview(input:{pullRequestReviewId, body})
            VALIDATION: Could not edit a review with a missing body.

    REST, the same PUT, on a review CREATED WITH a body
            200, state still PENDING, body replaced.

**The sender's suggested fix does not work.** GraphQL was the first
candidate, on the precedent that pending COMMENTS need it because the
REST PATCH 404s on them. It fails here with the identical message, so
the restriction belongs to the REVIEW rather than to one endpoint.
That precedent does not transfer, and implementing it would have cost
a cycle to discover.

Scope of that claim: the two UPDATE APIs above are what was tested.
Nothing here rules out some other operation setting a body while
leaving the review PENDING — it says only that neither documented way
of editing a review can.

**So the fix is at creation**, and only there: a pending review whose
body is non-empty from the start accepts the summary later and stays
PENDING. `create_summary_argv` (`pr_review.rs:1086`) already sends a
body; the loop does not take that path, because master's first call is
the skill's, which posts inline comments only
(`setup_assets/pr_review_skill.md:105-112`).

**A review already stuck has no repair this plan can vouch for.**
Deleting and recreating it loses the draft — the comments go with the
review, which this probe confirmed.

What is NOT established, and must not be written into the error: that
typing a summary in GitHub's interface fixes it. The probe says
nothing about the UI, and GitHub documents entering the summary as
part of SUBMITTING a review, not as a save-draft. If that is what it
does, the advice would publish a draft the whole handoff model exists
to keep unpublished — and a published review cannot be recalled.
`finish` would then find no pending review and open a fresh
summary-only one, which is not the in-place repair it was sold as
(codex 0644d5e).

So `finish` reports the limitation it can prove and stops: this review
was created without a body and neither update API can add one. It
preserves the draft, the local summary, the scratch and the watch, and
leaves recovery to the operator without naming a path nobody has
tried.

## The build

**The review is created with a body.** The skill's first call is what
makes the review, so that is where the body has to come from — a
summary if master has one by then, a placeholder if not. Nothing else
can reach it: `create_summary_argv` is not the path the loop takes,
and no later call can add a body to a review that has none.

**`finish` says what it knows when it meets a stuck one.** The 422
becomes a sentence naming the cause — created without a body, and
neither update API can add one — and says plainly that this draft
cannot be handed over automatically. It does NOT prescribe a repair;
see above for why the obvious-sounding one is a hazard rather than a
fix. Everything local survives, so nothing is lost while the operator
decides.

The wake is the actual damage: re-firing forever behind an undecoded
HTTP error is worse than a clear stop, so whatever `finish` does here
must end the loop rather than feed it.

**A test that would have caught it.** The existing one asserts a
vector of strings; the bug is in what GitHub does with it. Whatever
covers this has to exercise the request/response boundary against a
review shaped like the loop's — a deterministic fixture or an injected
runner, no live PR and no credentials — because a fake that returns
`Ok` from `set_summary` keeps exactly the blind spot that let this
ship (codex de070e6).

## Tests

- The review the loop creates has a non-empty body, so `finish` can
  set the summary on it and it stays PENDING — `finish` must not
  publish.
- `finish` against an ALREADY EXISTING empty-bodied review fails with
  a sentence naming the cause, not a bare 422, and leaves the draft,
  the local summary, the scratch and the watch intact. It must not
  claim a recovery path, and it must not leave a wake that re-fires.
- The request/response boundary is exercised, not the argv: a fake
  that answers `Ok` regardless must not be able to pass these.

## Out of scope

- The INSTALLED skill being out of date. The bundled source
  (`setup_assets/pr_review_skill.md`) is correct and already names
  `finish`; the copy at `~/.claude/skills/clank-pr-review/SKILL.md` is
  an older embedded version, which is why the sender was told to run
  `clank pr-review submit` — a verb that no longer exists. `clank
  setup` reports it as `upgrade` and would fix it. That is an install
  step on the user's machine, not a repo change.

  This plan DOES touch the bundled skill, because its creating call is
  the cause of the bug.
- #603 itself. Its pending review is untouched and stays that way.
