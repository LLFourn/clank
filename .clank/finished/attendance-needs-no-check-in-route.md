# attendance-needs-no-check-in-route
# Attendance quiets without a check-in route; its deadline is the backstop

## Problem

`turn_policy::qualifies` (crates/core/src/turn_policy.rs:302) refuses to quiet an attended task's already-shown work when `facts.check_in_route` is an error (`Unqualified::NoCheckInRoute`). The route is the typed pane check-in from `recovery::check_in_routes` (recovery.rs:2091). It errors for any session whose running version can't be established, e.g. "launch provenance is missing".

On 2026-10-26 the clank master attended a subagent with a standing attendance on record (`--expect 60m`). Even so, the same `continue` item was delivered at every turn-end, over and over, because its session had no typed route. The attendance was correct, but the rule ignored it.

This is a regression from `7a3a36be` (the-stop-hook-is-a-messenger, 2026-10-21). Before it, the hang check-in was armed inside the parked Stop hook (`check_in_when_due` returning `TurnDecision::Arm`), so it came back through the same wait that delivers all work, and attendance quieted for every session. The rewrite moved check-ins into the monitor's typed pane path and made quieting depend on that route.

The route was meant as a hang guard, and it isn't needed for that:
- **When the work finishes,** the harness's own completion notification ends the attendance (`completion_wake(tool)`).
- **If the work hangs,** the attendance's own deadline already ends quieting (`PastDeadline`), and owed work is then presented through the agent's parked wait. A typed check-in is a nicety on top, not the only way back.

## Change

- Remove the `check_in_route` condition from `qualifies`, along with `Unqualified::NoCheckInRoute` and `Facts::check_in_route`.
- Remove the plumbing that existed only to feed it: `Gathered.route`, `route_of`, the `Routes` threading through monitor.rs, and `Recovery::check_in_routes`. Each one goes only if nothing else reads it. Grep the class (route/check_in_route/check_in_routes) and report which hits were kept and why.
- The typed check-in itself (`may_type_check_in` and the check-in arming) is unchanged. It still runs where a typed route exists.
- **The deadline must actually wake.** When an attendance passes its `expect` while the agent's wait is parked and its owed work is quieted, the monitor re-serves that parked wait at the deadline without needing a repo change. If it doesn't do that today, schedule it on the monitor's existing timer path, not a new thread.

## Acceptance

- turn_policy tests:
  - a standing attendance whose route is unavailable now quiets received work;
  - past its deadline, the same attendance does not quiet it.
- The existing tests at turn_policy/tests.rs:1249 and :2299 (the routeless cases) are rewritten to the new contract, not deleted.
- A monitor-level test: a parked wait with quieted work and no typed route is presented its owed item once the attendance deadline passes, with no repo change. Use an injected clock or deadline, not a real sleep.
- Mutation check: putting the route condition back makes the first test fail. Removing the deadline re-serve makes the monitor test fail.
