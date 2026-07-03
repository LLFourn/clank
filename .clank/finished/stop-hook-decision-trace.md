# stop-hook-decision-trace
# Stop-hook decision trace: debug why a stop hook didn't wait

## Problem (lloyd)

When an agent stops and nothing happens, there is no way to tell WHICH of
two very different failures occurred:

1. the stop hook fired and OUR LOGIC decided not to wait (auto off, yield
   to background work, peek said busy, wait timed out, identity/config
   error), or
2. the stop hook was NEVER CALLED (hook not installed / wrong session /
   harness issue).

Today a non-waiting hook is mostly silent by design (`HookOutcome::Silent`
exits 0 with no output), so both cases look identical from outside.

## Fix: the hook writes a decision trace, every invocation

The stop hook (`cli/stop_hook.rs::run`) writes a single OVERWRITTEN debug
record before exiting — every invocation, whatever the outcome:

- Bound session → `.clank/agents/<label>/stop-hook.json`.
- Identity unresolved (Diagnostic before a label exists) →
  `.clank/stop-hook.json` at the repo level, so "fired but couldn't
  resolve who I am" is still distinguishable from "never fired".

Record contents (JSON, one object):
- `ts` — wall-clock write time.
- `tool`, `session_id`, `cwd`, resolved `repo`, `label` (when known).
- `last_assistant_message` — TRUNCATED (e.g. first 2000 chars) from
  `HookInput.last_assistant_message` (already on the input type; claude
  supplies it, codex omits → null). This is the correlator: if the
  agent's visible last output doesn't match the record, the hook did not
  run for THAT stop.
- `stop_hook_active`, `background_tasks` summary (count + whether one is
  a `clank wait`).
- `effective_auto`, `role`, `disposition` (YieldArmed/NeedsWorkCheck/
  NoBackgroundWork).
- `decision` + `reason` — see below.

Reading it: record absent or its `last_assistant_message`/`ts` doesn't
match the stop you're investigating → case 2 (hook never called). Record
matches → case 1, and `decision`/`reason` says exactly which branch.

## The architectural half: make the WHY first-class

`compute_outcome`'s "don't wait" exits currently collapse into a unit
`HookOutcome::Silent` — five distinct reasons are erased at the type
level: yield-to-armed-background-work, auto-mode off, busy-with-own-work
(peek said has-work), peek failed (fail-soft yield), wait timeout. The
trace must not re-derive these post-hoc; carry them:

- Either `HookOutcome::Silent { why: SilentReason }` (enum: `YieldArmed`,
  `AutoOff`, `BusyOwnWork`, `PeekFailed`, `WaitTimeout`, …), or a
  `StopHookReport` struct threaded through `compute_outcome` and written
  by `run` just before `emit_and_exit`. Lean: the enum — the wire
  behavior stays identical (Silent still exits 0 silently), but the
  decision is now data, and the trace serializes it. `Continue{reason}`
  and `Diagnostic{message}` record their existing strings.

Two-phase write so a crash is also diagnosable: write a minimal
"fired" record (ts + input context, `decision: "in-flight"`) right after
stdin parses, then overwrite with the full record at emit time. A file
left in-flight = the hook died mid-decision.

## Non-goals / notes

- No new surfacing in `status`/doctor yet — v1 is the file; `cat` it.
- Best-effort IO: a failed trace write must NEVER change the hook's
  outcome or exit code (swallow errors; the hook's job is the decision).
- `.clank/agents/<label>/` extras are gitignored by the allow-list —
  verify `stop-hook.json` stays untracked (like the agent's config.json).
- The record is per-agent and overwritten — no growth, no rotation.

## Tests (in-process, no binary spawn)

- Each Silent branch writes its distinct `reason` (drive
  `compute_outcome` with fixtures for auto-off, yield-armed, peek-busy).
- Diagnostic-before-identity writes the repo-level fallback record.
- `last_assistant_message` is truncated at the cap; absent → null.
- The trace write failing (unwritable dir) does not alter the outcome.
- Two-phase: after a full run the record's `decision` is final (not
  "in-flight").

## Acceptance

- Every stop-hook invocation leaves a fresh record at the expected path;
  the record distinguishes all non-wait reasons; the hook's wire behavior
  (exit codes, stdout/stderr shapes for claude/codex) is byte-identical
  to today.
