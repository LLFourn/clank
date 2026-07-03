# nudge-summarizes-background-tasks
# The stop-hook nudge summarizes background tasks instead of dumping commands

## Bug (lloyd, live in dark-skippy)

The `NeedsWorkCheck` + no-work nudge ("start `clank wait` as its own
background task") interpolates the RAW command line of every live background
task into the agent-facing reason (`nudge_reason`, stop_hook.rs:367:
`procs.join(", ")`). With real-world tasks — multi-clause `until …; do sleep
…; done; grep -iE "…" /private/tmp/...` one-liners — the nudge becomes a
wall of shell noise that buries the actual instruction. Pre-existing from
`b03fb0f [stop-hook-wait-alongside-background]` ("names the live
process(es)" — right intent, unbounded rendering).

## Fix

SIMPLIFIED (lloyd): don't echo the task at all — the agent doesn't need to
be told WHAT it backgrounded, only THAT something is still running. The
parenthetical command list goes away entirely:

- One task → "a background task"; N > 1 → "N background tasks". No command
  text ever reaches the message.
- The instruction text (start `clank wait`, run_in_background) is UNCHANGED
  — the spike-validated wording stays.

## Tests

- Pure `nudge_reason` cases: one task (singular wording), several tasks
  (count + plural), and NO command text leaking into the message even when
  tasks carry monster command lines; the instruction sentence byte-identical
  to today.

## Acceptance

- The nudge never contains task command text; it fits in a few lines and
  leads with the instruction; clippy baseline; suites green.
