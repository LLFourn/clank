# github-wake-pr-updates-and-pushes
# github wake sources: pr_updated and branch_push sub-kinds

`wait_events` github sources can't wake on the two highest-signal
events for a controller repo (lloyd, 2026-07-13): new commits landing
on a PR, and a branch being pushed. Both are already in the events
feed the poller consumes — the mapper just doesn't classify them.

## New sub-kinds (additive — old configs parse unchanged)

- **`pr_updated`** — `PullRequestEvent` with action `synchronize`:
  new commits on a PR. Fires on the BASE repo, so fork PRs are
  covered. Item: number/title/url from `pull_request`, like the other
  PR kinds.
- **`branch_push`** — `PushEvent` for BRANCH refs only:
  `payload.ref` must begin with `refs/heads/` (PushEvent also fires
  for tag pushes — `refs/tags/*` — and those are NOT branch pushes;
  they are ignored even when `branch_push` is subscribed, and never
  reach the branches filter, so the filter's semantics stay
  unambiguous — codex 3f04e3d). The short name derives from the ref
  only after that check. Item shape: `title` names the short ref and
  size (`main +3`), `url` is the compare link built from the
  payload's `before`/`head`
  (`https://github.com/<repo>/compare/<before>...<head>` — the
  actionable diff), `number` none.
- **Optional `branches` filter on the github source** (serde default
  empty = all): a `branch_push` wakes only for the named branches.
  Applies to `branch_push` only (PR kinds are number-scoped already);
  matched against the short ref (`refs/heads/` stripped).

## Care points

- PushEvent fires for EVERY push including the controller's own —
  the existing own-actions default filter already suppresses those;
  say so in the config docs.
- A push to a same-repo PR head fires BOTH PushEvent and (on sync)
  PullRequestEvent — two items for one push is correct behavior
  (different subscriptions), not a dedup bug; the seen-id cursor
  already prevents re-delivery of each.
- Action-gating discipline as before: `synchronize` only (not
  edited/labeled/etc.); recorded-shape fixtures for both new kinds
  in the fixture file.

## Acceptance

- Mapper fixtures: a recorded-shape branch PushEvent maps to
  `branch_push` with the compare url and short-ref title; a
  recorded-shape TAG PushEvent (`refs/tags/v1.0`) is ignored even
  when `branch_push` is subscribed with an empty branches filter; a
  `synchronize` PullRequestEvent maps to `pr_updated`;
  non-subscribed configs drop both; the branches filter admits/drops
  by short ref; own-actor filtering applies to the new kinds like
  the rest.
- Config: round-trip with the new kinds + `branches`; old configs
  (absent field, old kinds) parse unchanged.
- The skill's controller-repo section lists the new sub-kinds.
- fmt/clippy at the 18/6 baseline; suites green; no network in tests.
