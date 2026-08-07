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

In `clank status --tui`, pressing Enter on a github timeline row
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
  not backlog). Editing a source's config (kinds, branches, filters)
  starts a fresh log — same silent baseline.

## Realtime notes

`"delivery":"realtime"` events are logged and deduped through the same
inbox (the poll keeps running as the completeness backstop). A relay
delivery and its poll copy are ONE event — one inbox entry, one ack.
