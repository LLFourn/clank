# idle-compaction-keeps-the-wait
# Idle compaction keeps the wait

## Problem

Lloyd has reported this many times, and earlier plans scoped it out
(`a-nudge-goes-to-the-pane-named-by-its-agent`, Out of scope: "Fixing Claude's idle
compaction dropping the parked wait"). It is the complaint itself, and it needs
fixing at its source.

Observed 2026-10-29: `ruthless` in `~/src/frostsnap/.clank/worktrees/full-app-sim-driver`
(Claude Code 2.1.308, session `2e37a12d…`):

- 2026-10-28T00:27:25Z: `compact_boundary` (`compactMetadata.trigger: "auto"`),
  then the summary written as a `type: user` record (`isCompactSummary`, carrying
  the finished turn's own `promptId`), then
  "Compacted while idle, before the prompt cache expired".
- After that, no `clank wait` for ruthless existed: the compaction ended the parked
  `asyncRewake` Stop hook.
- clank's Claude reader (`recovery.rs`) read back a window that began after the
  original prompt, so it saw the summary as the start of a turn still running; `clank status` showed `ruthless → working` and recovery never
  nudged it. A review due on `fsim-genuine-on-522` sat undelivered for more than a
  day.

Two faults:
1. Compaction ends the parked wait, and nothing re-arms it.
2. clank reads the compaction summary as the agent starting a turn.

## Change

1. **Re-arm on compaction.** Claude runs the `SessionStart` hook after a
   compaction, with `source: "compact"`. clank already installs it
   (`clank stop-hook --tool claude --session-start`, `stop_hook.rs:251-262`).
   Establish from Claude Code's docs and its actual behaviour on 2.1.308 which
   hook can re-arm the wait: an `asyncRewake` hook on `SessionStart` with the
   `compact` matcher, a `PostCompact` hook if there is one, or whatever Claude
   offers. Then install it through `clank setup`, the way the Stop hook is
   installed. After an idle compaction the agent has a parked wait again, and
   pending work is delivered as usual.
2. **A compaction summary is not a turn.** In the Claude record reader, the
   `user` record that follows a `compact_boundary` (the summary, with
   `isCompactSummary` or the boundary's `logicalParentUuid` link; use whichever
   the real record carries) does not start a turn. An agent that was idle before
   the compaction stays idle, so if the re-arm in (1) ever fails, the existing
   idle recovery (and the `unwatched` rule) still nudges it.
3. Check whether Codex, OpenCode and Grok have an equivalent (automatic
   compaction while idle that ends the wait). Fix each one that does in the same
   way. Record any that can't be fixed, and why, in the finish message.

## Out of scope

- Turning off Claude's idle compaction (`idleCompaction: false`). Users keep it.

## Acceptance

- A Claude record fixture taken from the real shape above: an idle turn, then
  `compact_boundary`, then the summary `user` record, then the "Compacted while
  idle" system record. It reads as idle, not working, and recovery nudges it when
  work is pending. Mutation: treat the summary as a turn start, and the test fails.
- The installed hook config includes the compaction re-arm hook. A test drives
  the hook's handling of a compaction event: it parks a wait for the bound agent.
  The test must go through the same entry point the installed hook runs (no
  fixture that calls an inner helper).
- `clank doctor` reports a Claude setup that is missing the compaction hook, as
  it does for the Stop hook.
- No WHAT comments.
