# a-grok-stop-needs-no-deadline

Invariants: I1, I4.

## Why

Grok's hook cannot park. At a Stop it hands off to the monitor and waits up to `HANDOFF_BUDGET` = 10s (`grok_hook.rs:93`) for the answer. 9d130109 turned a missed budget into a notice that re-armed Grok's wait. 5410fb62 replaced that with a stderr Diagnostic, so a monitor slower than 10s now leaves Grok with no wait armed. Recovery then reads `HandsOff(CannotResume)` and the agent is stranded until a person steps in. The cause of the slow monitor on Linux CI was never traced.

Claude and Codex have a deadline too, and its consequences are wrong in the same way: at the deadline the hook goes idle instead of re-arming, and the doc comment at `stop_hook.rs:1080` claims otherwise. But it is `HOOK_TIMEOUT_SECS` (≈25 days, `setup.rs:316`) minus a margin, so it never fires in practice.

## Build

An unanswered handoff is a typed result end to end, not a Diagnostic that each adapter has to guess about.

1. **The child.** In `turn_child.rs`, a report that does not park and gets no verdict ends with a new `WireVerdict::Unanswered { why }` instead of `still(...)`/`unavailable(...)` Diagnostics. This happens two ways:
   - connected but slow: `Hand::Unanswered` at `answer_by`, the last word two seconds before the report's deadline;
   - not yet connected: attach attempts exhausted when `passes(by)` breaks the loop, including while a monitor is retiring.

   What stays a Diagnostic, being actionable and not a timeout:
   - a report that did not parse;
   - an outage the child gives up on early (`unavailable` from `outage.lost` returning `None`).

   A stale binding still ends silently, before any frame, as now. Real monitor verdicts pass through unchanged. Parking reports keep `expired_wait`. The frames between a hook and its own child are always one build's (the handoff contract), so this is no monitor protocol change.
2. **`stop_hook::answer_with`** passes `Unanswered` to the adapter's emit like any frame. The outer `HANDOFF_BUDGET` expiry in `answer_once`, the hook's own last resort, also yields `Unanswered`. So both layers end in the same typed result.
3. **Grok's adapter.** In `grok_hook::wire`, `Unanswered` becomes the fixed `ArmWait` continuation (`wake_render::ARM_WAIT`, as a blocking Stop reason), after the same `still_bound` check as any frame.
   - This hands the decision to the wait it arms, which asks the monitor when it connects. So an auto-off, blocked or quiet agent gets that answer from the monitor, as it would have on time.
   - No monitor-owned configuration is read in the hook.
4. **Other adapters.** Claude, Codex and OpenCode render `Unanswered` exactly as they render today's timeout Diagnostic, so their behaviour is unchanged. OpenCode's renewal path is untouched.
5. **Docs and registry.** Correct the stale doc comment at `stop_hook.rs:1080` ("a slow monitor never puts the agent to sleep") to say what is true per adapter. I1's `Scope` notes that a Grok Stop answered late still arms Grok's wait, naming the test below.

The Claude/Codex deadline machinery (`poll_deadline`, `VERDICT_GRACE`, the `Park` sentinel at the deadline) is not removed here. Its expiry runs through report deadlines, the child's last word and upgrade handoffs, so removing it is its own change. It never fires in practice (the ceiling is about 25 days). It is queued as `claude-codex-stops-have-no-deadline`.

## Acceptance

- Through the production path: `answer_once` → the real turn child → a test monitor socket that admits the report and does not answer within the budget. Grok's adapter writes the `ArmWait` continuation. A `clank wait` then connecting to that monitor, now answering, receives its work, or its quiet-policy answer for an auto-off agent.
- The same with no monitor ever attaching (attach exhausted): `ArmWait`.
- A stale binding at write time: silent, not `ArmWait`.
- An unparseable report: a Diagnostic, not `ArmWait`.
- A real verdict arriving in time is written unchanged.
- Claude's adapter given `Unanswered` writes what it wrote for the old timeout Diagnostic.
- Mutation: restoring 5410fb62's Diagnostic for Grok fails the first test.
