---
name: clank-github
description: Handle github_event wake items from clank's github wake sources — the event inbox, the react-then-ack loop, catch-up semantics, and realtime caveats. Load this when a clank wait wakes you with github_event items or when configuring github wait_events.
---

# Clank github events

`clank wait` can watch GitHub repos (`wait_events` entries with
`"kind":"github"`). Every event those sources ingest — whether it
arrived by polling or by the realtime webhook relay — is written to
YOUR per-agent event inbox (a write-ahead log under
`.clank/agents/<label>/events/`) BEFORE it is presented. Events stay
**unhandled** there until you explicitly mark them handled.

## The loop: react, then ack

1. A wake hands you `github_event` items (repo, event kind, number,
   title, actor). When an item carries an indented `↳ …` line (or an
   `instructions` field in JSON), that is the OPERATOR'S STANDING
   INTENT for this watch — follow it as the definition of "react".
   Otherwise triage on your judgment — `gh pr view`, `gh issue
   view`, your own tooling — and do whatever the event demands.
2. When an event needs nothing more from you, mark it handled:

   ```sh
   clank events ack <id>        # ids from `clank events list`
   ```

3. Re-arm your wait and stop.

**Unacked events wake you again on every arm — by design.** The inbox
is at-least-once: if your session crashes mid-reaction, or the wait
dies between delivery and your turn, the event re-presents until you
ack it. Expect re-presentation of things you half-handled; reacting
idempotently and acking promptly is the discipline.

## Inspecting the inbox

```sh
clank events list            # unhandled events
clank events list --all -j   # everything, machine-readable
clank events show <id>       # one full record (url, transport, age)
```

`list`/`show` display the watch's `prompt` (its standing
instructions) with each event — including events logged before the
prompt was written: it is joined from the CURRENT config at render
time, never stored in the log.

In `clank tui`, pressing Enter on a github timeline row
opens the EVENT PAGE: the same facts plus every agent's copy, with
`open in browser` and `ack` actions (ack marks every copy handled —
the same fanout as `clank events ack` per copy).

Ids are the listed seq (qualified as `<source>@<seq>` when several
watched repos collide — the error message shows the qualified forms).

## Catch-up semantics

- Events that fire while NO wait is armed (agent offline, wait killed,
  the gap between delivery and re-arm) are caught up on the next arm:
  the first poll reconciles GitHub's event feed against the inbox and
  presents what you missed.
- The feed only retains ~300 events; a longer outage logs one loud
  "may have been missed" warning — treat it as a cue to sweep the
  watched repo manually.
- The very first arm of a new source baselines silently (history is
  not backlog). Editing a source's config (kinds, branches, the PR
  scope, filters) starts a fresh log — same silent baseline. Adding
  an EMPTY `prs` list changes nothing and keeps the log.

## CI conclusions are read, not received

`ci_passed` / `ci_failed` do not come from the event feed — GitHub
does not put check results there. They are READ: every watched PR's
head is asked what its checks say, and the answer is reduced to one
result for the PR (a check suite is one App's view, and a repo's
checks routinely come from several, plus the older commit statuses).

Two consequences worth knowing:

- **There is no backlog to catch up on.** An arm sees the CURRENT
  result. A build that went red and was replaced by a green one while
  nothing was armed is not recoverable — the red is simply gone.
  Nothing watches on your behalf between waits: the watch runs for
  exactly as long as your `clank wait` does, so an armed wait is the
  only thing that can see a transition. `prs` does not change that —
  it bounds WHICH PRs are read, and so how many requests each poll
  costs.
- **An unchanged build is silent.** The same conclusion on the same
  head is announced once; a new push is a new head and so a new
  result. A re-run going red → green → red announces red once, not
  twice.

With `"delivery":"realtime"` a finished check suite makes the checks
be read straight away instead of at the next poll — unless they were
read moments ago. Cues arriving in quick succession are DROPPED, not
queued (several apps finishing at once would otherwise buy a round of
requests each), so a conclusion can still wait for the next scheduled
read. The webhook is only a cue: the conclusion always comes from the
same read, so realtime and polling watches cannot tell you different
things.

## Realtime notes

`"delivery":"realtime"` events are logged and deduped through the same
inbox (the poll keeps running as the completeness backstop). A relay
delivery and its poll copy are ONE event — one inbox entry, one ack.
