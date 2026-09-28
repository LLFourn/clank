# a-review-event-carries-its-own-link

## Goal

A GitHub event in the TUI must always be openable. Today an inline review comment records no title and no URL, so when its object is gone the page is a dead end, and when the object is still there the operator still has nothing to click.

## Problem

Observed on `frostsnap/frostsnap#602`: the event page shows `no longer on GitHub (deleted, or not visible to this token)` and then `event pr_comment (review_comment)`, `age`, `number #602`, `actor LLFourn`, `copies` — and nothing else. No body, no title, no link.

The message is **correct**, not a lie: `GET /repos/frostsnap/frostsnap/pulls/comments/4118128491` really does 404, the id is not an issue comment or a review either, and PR 602 currently has zero review comments, so that object was deleted after the event was recorded. `state_from_status` (`status_tui/event_content.rs:63`) is right to call 404 terminal. The defect is that when the fetch cannot help, the record has nothing left to offer.

The record is impoverished at classification time, and the cause is specific to ONE of the two transports. Checked live against the repo Events API, which is what the poll reads:

| event type | `payload` container | `title` | `html_url` | the object's own link |
| --- | --- | --- | --- | --- |
| `IssueCommentEvent` | full issue, ~30 keys | present | present (the issue) | `payload.comment.html_url`, anchored `#issuecomment-…` |
| `PullRequestReviewCommentEvent` | **stub: `base, head, id, number, url`** | **absent** | **absent** | `payload.comment.html_url`, anchored `#discussion_r…` |
| `PullRequestReviewEvent` | **stub: `base, head, id, number, url`** | **absent** | **absent** | `payload.review.html_url`, anchored `#pullrequestreview-…` |

`classify_event` (`github/feed.rs:233-248`) reads `title` from `obj.title` and `url` from `obj.html_url`, where `obj` is `pr()` — that stub. So **every POLL-sourced inline review comment and submitted review carries `title: null, url: null`**. The WAL row behind the screenshot is one (`transport: "poll"`). The stub's `url` is the *API* url (`api.github.com/...`), so it cannot be substituted for the missing `html_url`.

**There are two classifiers, not one.** `classify_event` (`feed.rs:102`) serves the poll through `map_page`; `classify_webhook` (`feed.rs:390`) is separate, builds its own `WaitItem::GithubEvent`, and reads `obj.title` / `obj.html_url` at `feed.rs:507-515`. Changing one does nothing for the other. They also see different payloads: a webhook delivers a FULL `pull_request`, as this repo's own webhook fixtures show (`{"number":1,"title":"t","html_url":"u"}`), so webhook-sourced review rows do carry a title and a container URL. The null rows are the poll's.

The **anchored link** is a separate defect and it is transport-independent: both classifiers record the CONTAINER's `html_url`, never the object's own. That is the improvement both need.

The link the operator wants is in the payload and is being thrown away. It is also **better** than the container's link, because it is anchored at the object. The issue-comment family has the same smaller flaw on both transports: it records the issue's `html_url`, so opening it lands at the top of a long thread instead of at the comment.

The deeper point is that the page treats "I could not fetch the object" as "there is nothing to show". The container is always addressable — the stub has `number`, and the event kind already says whether it is a PR or an issue — so the page can always offer *the PR*, fetch or no fetch. That also repairs every row already on disk, which a classifier fix alone cannot.

## Work

- Record the URL of the object the event is **about**: `payload.comment.html_url` for both comment families, `payload.review.html_url` for a review, falling back to the container's `html_url` when the payload has no object link. Never the stub's `api.github.com` url — that is not something an operator can open.
- Apply that through ONE shared subject-URL rule that both classifiers call, so the poll and the webhook cannot drift into disagreeing about where an event's link comes from. Updating them separately is acceptable only if the plan says why one rule would not serve both; two copies of this decision is how they diverged in the first place.
- Leave `title` absent when the payload does not carry one rather than inventing a placeholder. `#602` is already the address; a fabricated title is worse than none. If a title is wanted for review events it must come from a real fetch, and that is a separate decision, not a classifier change.
- Make the event page offer the container whenever it knows the repo and number, and especially when content is `Unavailable` or absent. A deleted review comment still belongs to a PR, and "open #602" is the useful answer where a dead end is now. Derive the address from the kind — pull for the PR families, issue for the issue families — not by guessing from the number.
- Give the container its OWN page action rather than folding it into the existing open. `event_open_url` (`status_tui/input.rs:1757`) resolves a single URL for a single `Open` effect, preferring the fetched object's over the record's, so one row cannot offer both the anchored comment and its PR when both exist — and on a `Ready` event both usually do. Changing which one that function prefers trades one loss for another; a second action is what lets a reader go to the comment or to the thread it lives in.
- Derive that container link **at render time**; do not rewrite the log. The event WAL is append-only and durable, and rows written before this plan have no URL to migrate. Deriving repairs them for free, and is the reason to prefer it.

## Acceptance

- A `review_comment` and a `review` event record the anchored URL of the comment or review, not the container's and not an `api.github.com` address — asserted for BOTH classifiers, each against its own real payload: the Events API's 5-key stub (`base, head, id, number, url`) for the poll, and a full `pull_request` for the webhook. A stub fixture fed to the webhook classifier, or a full-PR fixture fed to the poll's, proves nothing about the shape that transport actually receives.
- An `issue_comment` event records the comment's anchored URL, and an event whose payload carries no object link still records the container's.
- `title` is absent, not empty or invented, for events whose payload has no title, and the page reads correctly without one.
- The event page offers a link to the container for any event that knows its repo and number — including one whose content is `Unavailable`, and including a row recorded before this change with `url: null`. The dead-end page from the report is reachable in a test and is no longer a dead end.
- On a `Ready` event that has both, the page offers BOTH the anchored object and its container, as two actions resolving to two different URLs. Asserting only the `Unavailable` row would pass with a page that still drops one of them whenever the fetch succeeds.
- The `Unavailable` wording is unchanged and still terminal; this plan adds a route onward, it does not make a deleted object retryable.

## Scope

Classification and presentation only. No change to the fetch, to `ContentRef` addressing, to `state_from_status`'s 404/410 rule, or to the WAL format. Out of scope: fetching titles for review events, and any backfill or rewrite of existing event logs.
