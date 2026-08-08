# tui-github-event-content

Show the actual PR/issue/comment body on the github event page, so
triage does not require leaving the TUI.

Split from `tui-github-event-content-and-open`; the inert-hotkey half
is now `tui-event-page-hotkeys`, which is small, verified, and
independent. What remains is the content fetch, and the findings
below are why it is not a small plan.

## Finding 1: the identity is ALREADY extracted — just not persisted

An earlier revision of this note claimed no comment id is captured
anywhere. That was wrong (codex on b86c78d). `action_key`
(github_events.rs:258-298) already reads
`payload[container]["id"]` and namespaces it by subtype — `review`,
`review_comment`, `pr_issue_comment`, `issue_comment` — precisely so
equal numeric ids in different resource domains cannot collide.

So the identity exists at ingest. What is missing is a PERSISTED,
typed form of it: today it is consumed into a formatted dedup
string. Recovering it by parsing that string back would couple
fetching to dedup and make the key's format load-bearing in a second
place, which is how dedup gets changed by accident.

What remains true: the record's `url` is the issue/PR html_url
(github_events.rs:233-245, 441-449) because comment events classify
against the issue/PR object, so the canonical comment anchor still
has to come from the new reference.

## Contract 1: a separate reference, dedup untouched

`action_key` MUST stay byte-for-byte identical. It is the dedup key;
any drift re-fires already-acked events. Persist the reference
ALONGSIDE it, never derived from it.

Add one optional, serde-defaulted, typed field to
`WaitItem::GithubEvent` (core/wait.rs:139) — absent on every legacy
row, and absent from serialization when absent, so existing WAL
bytes stay byte-stable. Variants and their endpoint inputs, which
are NOT uniform:

- `IssueComment { id }` → `/repos/{repo}/issues/comments/{id}`
- `PrIssueComment { id }` → same endpoint, distinct variant because
  the namespaces differ and the page copy differs
- `ReviewComment { id }` → `/repos/{repo}/pulls/comments/{id}`
- `Review { number, id }` → `/repos/{repo}/pulls/{number}/reviews/{id}`
  — note this one needs the PR NUMBER as well as the id; a
  reference carrying only an id cannot address it
- `Pr { number }` / `Issue { number }` → the object itself, for
  open/update events

Canonical URL comes from the same reference, falling back to the
record's `url` when it is absent.

## Contract 2: deterministic merge across legacy and new rows

A merged timeline component can hold legacy rows (no reference) and
new rows (with one), or rows whose references disagree. State the
rule rather than letting row order decide:

- The component's reference is the first one in the members' EXISTING
  provenance order — the same first-some rule every other merged
  field already uses, which is total and therefore stable across
  refreshes. (This said "newest" when written; that was a second,
  arbitrary ordering for no gain. Implemented as first-some for
  consistency with the surrounding merge.) Legacy rows never clear a
  reference a sibling carries.
- If two rows carry DIFFERENT references, the component does not
  speak for both: the page uses the reference of the row it is
  actually targeting (the retained-set target), not a merged guess.

Both cases need a test; the mixed legacy/new component is the one
that will occur in every existing repo on the first run after this
lands.

## Finding 3: the plumbing already exists — corrected

An earlier revision of this note claimed the status TUI is a
synchronous loop needing "a background worker, a channel, and
content state". That was wrong, and it overstated the work.

`run_tui` (status_tui/mod.rs:1155) is an `async fn` running inside
the CLI's tokio runtime, and the loop already multiplexes an `Ev`
enum over an mpsc channel fed by dedicated threads — one for
watcher refreshes, one for WINCH — draining and COALESCING each
burst into a single repaint (mod.rs:1180-1199, 1478-1490).

So a fetch is the same shape as what is already there: spawn it,
send an `Ev` when it lands, let the existing drain repaint. The
worker and the channel are not new; only the content state and its
lifecycle are.

Reuse the existing injectable async transport (`get` /
`get_or_acquire` in github_events.rs) rather than shelling `gh api`
per view. The decisive reason is TESTABILITY, not tidiness: the
transport is injectable, so the fixture acceptance below can be
driven with no network and no `gh` on PATH. A subprocess per view
would make those fixtures need one or the other.

The genuinely new work is state, not plumbing: loading /
unavailable / retryable-error states that survive `apply_refresh`
and the retained-set retargeting the event page already does.

## Contract 3: staleness is keyed, not hoped

Every fetch state is keyed by (stable reference, request
generation). A completion whose generation is not the page's current
one is DISCARDED — after retained-set retargeting, after the page
closes, and after any refresh that re-aims the page. Without this a
slow response lands on whatever event is open when it arrives, which
is the exact bug of showing one event's body on another's page.

## Contract 4: retry is an explicit operator action, and nothing else

**Decided (codex on cd549ee): an explicit retry action on the page,
offered ONLY in the retryable-error state. Snapshot refreshes never
re-issue a failed fetch.**

The alternative — retry on the next refresh — is not merely less
tidy, it is unbounded. The TUI refreshes on every filesystem watcher
event, so in an active repo a persistently failing fetch would
re-issue continuously for as long as the page is open. That spends
the rate limit of the SAME `gh` token the github wake sources poll
with, so a broken event body could degrade wakes repo-wide. An
operator-triggered retry cannot do that.

Use the hotkey seam from `tui-event-page-hotkeys`: the action is
availability-gated, so its key is live exactly when a retry is
possible and inert otherwise — the page never advertises a key that
does nothing.

Retryable vs terminal must be distinguished, or the retry action
becomes noise on content that will never load:

- **Terminal / unavailable** — 404 and 410 (deleted or invisible
  comment). Say so and offer NO retry.
- **Retryable** — transport failure, 5xx, and rate-limit responses.
  Offer the action.

The first fetch when the page opens is of course automatic; the ban
is specifically on re-fetching after a failure without the operator
asking.

## Scope

- Capture the reference at ingest per Contract 1, leaving
  `action_key` untouched.
- Fetch the most specific object through the existing transport,
  delivered as an `Ev` like every other async input (Finding 3).
- Render it as the page's primary region, scrollable, with loading /
  unavailable / retryable-error states retained across refreshes and
  retargeting per Contract 3.
- Derive the canonical URL from the reference, falling back to the
  record's `url`.

## Acceptance

- `action_key` output is byte-for-byte unchanged, asserted against
  the existing key fixtures — dedup must not move.
- Events logged BEFORE this change still decode and still render,
  degrading to the issue/PR URL with no body; WAL bytes for such
  events are unchanged.
- A merged component mixing legacy and referenced rows resolves per
  Contract 2, and one with conflicting references targets the
  selected row rather than guessing.
- A result arriving AFTER a retarget is discarded, not displayed —
  driven deterministically (inject the late completion), not by
  racing a real fetch.
- Fixtures for PR, issue-comment, PR review and PR review-comment
  events yield the right body and canonical URL, including multiline
  markdown and deleted/missing content, driven through the
  injectable transport with no network and no `gh` on PATH.
- Repeated unrelated snapshot refreshes while a fetch is in the
  failed state issue NO further requests — asserted by counting
  requests through the injectable transport, not by inspecting the
  rendering.
- The retry action is offered ONLY in the retryable-error state:
  absent while loading, absent on success, and absent on a terminal
  404/410, which reports unavailable instead.
- A failed fetch followed by the operator retry produces a body; a
  slow or failing fetch never freezes or closes the TUI.
- Existing ack fanout, retargeting, prompts and refresh behaviour
  unchanged.
