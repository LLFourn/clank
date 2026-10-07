# a-watch-says-what-it-watches
# A source row and its agent list say what each watch is subscribed to

## Why

In `~/src/fs-war-room` the TUI's SOURCES panel shows:

```
🐙 frostsnap/frostsnap  @claude,@claude,@claude  3 hosted
```

and Enter opens a list of three identical rows: `@claude  hosted`. `claude` has three github
watches on `frostsnap/frostsnap`, each subscribed to different events:

- `pr_opened`, `issue_opened`, `pr_merged`
- `pr_comment`, `review_submitted`
- `pr_updated`

The panel names a github source by its repo (`ConfiguredSource::of` sets `name = g.repo`)
and groups rows by (backend, name), so all three fall in one row. The row then lists the
agent once per watch, and the agent list has nothing to tell the watches apart. The user's
question: "Why doesn't it show what it's subscribed to."

Three problems:

1. **A watch is defined by its subscription, and nothing shows it.** Two watches on one
   repo differ in what they select: event kinds, the PRs or branches a kind is scoped to,
   and whether the watcher's own actions are admitted, per clause in a nested `watch`.
   The display drops all of it.
2. **The agent list repeats an agent per watch.** `@claude,@claude,@claude` names one agent
   three times.
3. **"hosted" says nothing for github.** The monitor publishes no lifecycle for a github
   watch, so a hosted one reads "hosted", and the row says "3 hosted".

## What to build

Identity, subscription and state stay three separate things. A source is still identified
by (agent, source key) for selection, navigation, events and restart. The subscription is
a description of its config. The state is what the monitor reports. Nothing here re-keys a
log.

- **Each github source describes its subscription, from `Selection`.** Add a describe
  method to `github::selection::Selection` that renders its clauses, so the renderer never
  re-derives a selection predicate. Each clause reads as its event kinds, then the scope
  that clause actually has, then its own-actions rule when that rule changes admission:
  - A flat watch desugars as `Selection::flat` already does. `prs` scopes only the
    PR-scoped kinds, never `pr_opened`, and `branches` scopes only `branch_push`. So
    `events: [pr_opened, pr_comment], prs: [522]` reads `pr_opened; pr_comment on PR
    #522`, not a suffix claiming both are limited to #522.
  - A nested clause reads with its own scope, so `pr.numbers` covers `pr_opened` too
    when the clause names it. It also reads with its own include-own override when that
    differs from the default.
  - Own actions show as `incl. own` when admitted for a kind the actor filter applies to
    (`is_filterable_by_actor`), and nothing otherwise. `review_submitted` is never
    filtered, so it never carries the note.

  Clauses render in `Selection`'s canonical order. Two flat watches with reordered events
  but the same admission read the same: they ARE the same subscription. They are still two
  sources with different keys, and the source page shows the key, which is what tells them
  apart. A command source has no subscription line; its name is what it is.
- **A github source states its configured delivery, not observed health.** With no
  lifecycle from the monitor, a hosted github source reads its CONFIGURED delivery:
  `poll every 120s (configured)` or `poll every 60s (default)`, and for realtime
  `realtime, poll every 120s as backstop`. Realtime front-runs the same poll loop, which
  stays the completeness backstop and the fallback when the relay fails. The interval is
  the configured one parsed by the existing duration helper, and the server's
  `X-Poll-Interval` may slow it (`feed::effective_interval`). The page says "at least"
  where it gives the cadence. The existing diagnostics win over the delivery label:
  "monitor not running", "not hosted", and the rejected-config suffix read exactly as
  today.
- **The agent list shows each member's subscription and state.** A member row reads
  `@claude  pr_comment, review_submitted · incl. own  poll every 120s (configured)`. The
  source page shows the subscription as its first fact, and its key.
- **The panel row names each agent once,** with a count when one agent has several sources
  in the row: `🐙 frostsnap/frostsnap  @claude ×3`. For github rows whose members all read
  the same state, the state is shown once. Command rows keep exactly their current text.

## Acceptance

- fs-war-room's shape as a fixture: one agent, three flat github watches on one repo
  (`pr_opened, issue_opened, pr_merged`; `pr_comment, review_submitted`; `pr_updated`). The
  panel row reads `@claude ×3`. The agent list has three rows, and each row's assertion
  names its own events.
- Subscription semantics, each a test against the description:
  - a flat watch with `prs` scopes its PR kinds and not `pr_opened`;
  - a flat watch with `branches` scopes `branch_push` only;
  - two watches differing only in own actions read differently;
  - a nested clause's `pr.numbers` scopes its `pr_opened`;
  - a nested clause's include-own override shows against the default;
  - two flat watches with the same events reordered read the same and keep different
    keys.
- Delivery wording: poll default, poll configured, realtime with its poll backstop, and
  the server floor named on the page. "monitor not running", "not hosted" and a rejected
  config read as before.
- A command source's row and agent list text are unchanged.
- No github row or page reads a bare "hosted".
- Mutation-checked: dropping the subscription from the list, scoping `pr_opened` under a
  flat `prs`, dropping the own-actions note, repeating the agent, and the delivery label
  overriding "not hosted" each fail a test.

## Out of scope

How watches are keyed, polled, relayed or merged; relay health telemetry; the grouping of
rows by repo.

## As built

- `Selection::describe` renders the watch's own clauses. Clauses with the same scope and
  own-actions rule admit exactly the union of their events, so they read as one. Without
  that merge, the flat shape's desugaring showed `issue_opened, pr_opened; pr_merged`.
  The rest stay separate, in canonical order. Each clause gives its sorted selectors, then
  `on PR #…` or `on branch …` for the scope it actually has, then `· incl. own` when it
  admits own actions for a kind the actor filter applies to.
- `feed::configured_delivery` (with `DEFAULT_POLL_INTERVAL`, now shared with
  `effective_interval`) states the configured delivery through `parse_duration_str`.
  `ConfiguredSource` carries both strings for github sources, computed once in
  `ConfiguredSource::of` on the refresh path.
- `source_state` reads a hosted github source's delivery where it read "hosted". "monitor
  not running", "not hosted" and the rejected-config suffix are unchanged and take
  precedence. A github row with several sources shows their one state once, or counts the
  distinct full states. Command rows keep their counted words.
- The panel row names each agent once with `×N` (`row_agents`). The agent list adds each
  member's subscription. The source page leads with `subscribes` and `key`, and notes that
  GitHub's `X-Poll-Interval` may make it poll less often.
