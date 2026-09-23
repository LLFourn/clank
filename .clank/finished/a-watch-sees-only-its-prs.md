# a-watch-sees-only-its-prs

A watch that names its PRs is woken about those PRs and nothing else.

> "All the github watching stuff for comments/ci/updates etc need to
> be able to be limited to a list of PRs that are in scope for that
> agent. Of course events like 'new pr' can't be limtied." — lloyd

## Why

`GithubSource.prs` exists, and it bounds exactly one thing: which PRs
the CI reader fetches checks for (`a-red-build-is-a-wake`). Every
other PR-shaped kind ignores it, so an agent that is looking after PR
7 is still woken by a comment on 40, a push to 31, and a merge of 12
in the same repo. On a busy repo that is most of its wakes, and each
one costs the agent a turn to decide it does not care.

`branches` already set the shape for this: a `branch_push` watch that
names branches is woken for those branches. A PR list is the same idea
for the kinds that name a PR, and the field is already in the source
identity so a narrowed scope gets its own log rather than reading a
cursor baselined for a wider one.

## The build

**The scope applies to the kinds that name a PR.** `pr_updated`,
`pr_merged`, `pr_comment` (all three of its classes — an issue comment
on a PR, a submitted review, an inline diff comment) and the two CI
kinds. Every one of them already carries the PR number in BOTH payload
forms — the feed's and the webhook's — so the filter is one rule
applied at the two classifier sites where `branches` is applied today,
and the two forms cannot diverge.

**The kinds that name no PR are untouched, and the docs say so.**
`pr_opened` cannot be scoped: a PR that does not exist yet is not in a
list written before it. `issue_opened`, `issue_closed`,
`issue_comment` are not about PRs at all, and `branch_push` has
`branches`. A watch that lists `prs` alongside `pr_opened` gets every
new PR and its named PRs' activity — that is the useful combination
for a controller agent, and it should read as deliberate rather than
as the scope failing.

**CI keeps scoping what it FETCHES.** For the feed kinds the list is a
filter on events that arrive anyway; for CI it also chooses what to
ask GitHub about, which is what keeps the request count down. Same
field, two jobs, and the field's doc has to keep saying both.

## Open question

A PR-scoped event whose payload carries no number cannot be placed in
or out of scope. Dropping it silently is the failure a scope exists to
prevent being noticed; emitting it defeats the scope. The
recommendation is to drop it with ONE loud line naming the kind and
the repo, the way this module already reports an overrun — decided in
the intro, not in passing.

## Tests

- With `prs: [7]`, a comment, a review, an inline comment, an update
  and a merge on 7 all wake; the same five on 8 do not.
- With `prs` empty, all of them wake — the scope is opt-in.
- `pr_opened` fires for a new PR regardless of the list, and
  `issue_*` / `branch_push` are unaffected by it.
- The feed form and the webhook form of one event agree about scope
  (the existing both-forms test, extended).
- Narrowing the list re-keys the source; an empty list does not.

## Out of scope

- Any scope that is not an explicit list (authored-by-me, assigned,
  label-matched). A list is what was asked for, and every other rule
  is a query against GitHub rather than a filter on a payload.
- The kinds themselves, and anything about what a wake item carries.
