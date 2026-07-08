# stop-hook-claude-loop-visibility

Research whether clank's Claude stop hook can observe in-progress
"loops" or other self-scheduled work that the Claude agent has set
up, and whether that signal is reliable enough for clank to stop
nagging the master while the loop is running.

## Scope

This is a research spike only. Do not implement backoff behavior in
this plan. The output is a short written answer with evidence.

## Questions

- What data does the Claude stop hook receive when Claude Code is in
  an in-progress loop or has scheduled follow-up work?
- Does that data distinguish a Claude-managed loop from ordinary idle
  completion, blocked work, or background shell processes?
- Can clank observe the signal from its existing stop-hook code path,
  or would it require a new integration point?
- If observable, is the signal stable enough to use as a suppression
  condition for master nudges, similar to the existing background
  process backoff?

## Method

- Inspect the current clank stop-hook implementation, traces, and any
  recorded hook payloads or docs in the repo.
- If local evidence is insufficient, run a minimal manual experiment
  that captures the stop-hook payload/state for a Claude loop without
  changing production behavior.
- Compare the loop signal to the existing background process backoff
  mechanism.

## Acceptance

- The plan answers "yes", "no", or "unknown without upstream support"
  with concrete evidence.
- If the answer is "yes", the note names the exact field/state clank
  can use and the false-positive/false-negative risk.
- If the answer is "no" or "unknown", the note names the missing
  signal and the smallest future experiment or upstream request that
  would resolve it.
- No implementation changes beyond the research note.

---

# RESEARCH NOTE (the deliverable)

## Answer: YES — via `session_crons`, already on the hook's stdin

Claude Code delivers a `session_crons` array in the Stop hook stdin
payload (added in v2.1.145, alongside `background_tasks`). Per the
official scheduled-tasks documentation, it carries BOTH recurring
session crons (CronCreate, fixed-interval /loop) AND pending one-shot
wakeups scheduled via ScheduleWakeup (dynamic /loop): "The pending
wakeup appears in `session_crons` in Stop hook input"
(code.claude.com/docs/en/scheduled-tasks; corroborated by the
v2.1.145 changelog). Clank's `HookInput` already receives the field on
the same stdin and deliberately ignores it (hook_io.rs lists it among
the "claude-only extras") — the E1 captured payload in hook_io.rs's
tests shows `"session_crons": []` arriving. So NO new integration
point is needed: parsing one more `#[serde(default)]` field on the
existing `HookInput` is the whole hookup.

## Live probe evidence (this session, 2026-07-08)

Two controlled stops with the production hook (traces in
`.clank/agents/claude/stop-hook.json`):

1. **Wakeup pending + a live subagent** (04:23:52): `background_tasks:
   1`, disposition `needs_work_check`, silent `busy_own_work`. The
   count was the SUBAGENT — confounded, which motivated probe 2.
2. **ONLY a pending wakeup, nothing else** (04:28:17):
   `background_tasks: 0`, disposition `no_background_work`, decision
   `continue` (`arm_wait_nudge`). **A pending ScheduleWakeup is
   invisible in `background_tasks`** — it does not suppress the nudge
   today. This confirms the docs' separation: `background_tasks` is
   live shells/subagents only; scheduled work rides `session_crons`.

Bonus observation (once, not load-bearing): probe 2's exit-2 block
never arrived as a continuation — the scheduled wakeup superseded it
and the session woke with the wakeup prompt instead. So today's nudge
appears to LOSE the race against a pending wakeup rather than break
the loop; harmless either way, but suppression would make it clean.

## Reliability as a suppression condition

- **Fit with the existing backoff**: same shape as the background
  process mechanism — `session_crons` non-empty could map to a new
  `BgDisposition`-style yield (the wakeup/cron re-invokes the session,
  so yielding is safe the same way `YieldArmed` is).
- **False-positive risk (the real one)**: `session_crons` mixes
  one-shot wakeups (expire after firing) with RECURRING crons (never
  expire). Suppressing on "non-empty" would let one unrelated
  recurring cron permanently silence master nudges — a stalled-master
  failure mode. A safe rule needs the ENTRY SHAPE (one-shot vs
  recurring, next-fire time), and the array's schema is NOT documented
  in the hooks reference.
- **False-negative risk**: low — the docs state the pending wakeup is
  present at Stop time.

## Limitation hit during research

Capturing the RAW payload (to pin the `session_crons` entry schema)
requires adding a temporary stdin-tee Stop hook to
`~/.claude/settings.json`; the permission classifier denied that
self-modification, correctly, without explicit user approval. The
documented behavior + probe traces were sufficient for the questions
above, but not for the array's field names.

## Smallest next step (if suppression is wanted)

One user-approved capture: temporarily add an additive
`cat > /tmp/stop-capture.json` Stop hook entry, end a turn with (a) a
pending ScheduleWakeup and (b) a recurring CronCreate cron, and read
the two `session_crons` shapes. That pins the one-shot-vs-recurring
discriminator; the suppression rule then keys on one-shot entries (or
entries with a near next-fire), parsed fail-soft like every other
HookInput field.
