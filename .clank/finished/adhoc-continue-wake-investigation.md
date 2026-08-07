# adhoc-continue-wake-investigation
# Investigate: CONTINUE on an ad-hoc commit does not wake the master's wait

## Report

An agent reports that writing a CONTINUE verdict on an AD-HOC commit
(a commit outside any plan) does not wake the master's parked
`clank wait`. For plan commits, gate-continue produces the master's
`continue` work item and the wake; for ad-hoc, apparently nothing.

## Prime suspect (verify first)

`WorkStatus::work_for`'s ad-hoc arms route exactly two states:
`Unreviewed` → reviewers get `AdHocReview`, `ChangesRequested` →
master gets `AdHocRevise`. A CONTINUED ad-hoc gate routes NOTHING —
so the master is not woken by design of the current arms, and the
question is whether that design is right:

- For a PLAN, "continued (not FINISHED)" means the master should
  keep working — the plan defines what "continue" means.
- For an AD-HOC commit there is no plan to continue; but the master
  may still want the wake as an acknowledgment that review passed
  (e.g. to proceed with a dependent step it was holding), and the
  reporting agent evidently expected one.

## Investigation

- Reproduce: bare repo, master + commit reviewer, ad-hoc commit,
  reviewer writes CONTINUE — observe the master's parked wait (does
  the fold refresh produce any item? does the wait wake and return
  empty, or not wake at all?). Distinguish "no ITEM by design" from
  "no WAKE due to a watcher gap" — the wait's watcher may not even
  refold on the feedback write for ad-hoc (plan feedback paths are
  watched; verify the ad-hoc feedback file is inside the watched
  tree).
- Decide the semantic: either (a) a continued ad-hoc gate produces a
  master item (e.g. `AdHocContinued { sha }` — "review passed;
  nothing further owed") exactly once (idempotence: what suppresses
  it after the master sees it? there is no ack surface for master
  items — this needs a real design, likely keyed off the snapshot
  the wait armed with, like `detect_finished`), or (b) the current
  no-item behavior is correct and the TUI/status must make the
  continued state legible so agents stop expecting a wake (the
  status bar already shows ad-hoc gates since
  tui-adhoc-review-activity).
- Check the SAME question for `Finished` verdicts on ad-hoc commits
  while in there (the other terminal outcome).

## Acceptance

- A written reproduction (test) of the reported scenario pinning
  today's behavior, whichever way the decision goes.
- The decision recorded in this plan with the reasoning; if (a), the
  item + once-only delivery implemented with deterministic tests; if
  (b), the legibility gap named and closed (status/TUI surface, or
  skill teaching).
- No change to plan-commit wake behavior.

## FINDINGS + DECISION (recorded 2026-08-09)

**Reproduced and pinned** (`adhoc_gate_routing_matrix_…` in core
wait tests): `work_for`'s ad-hoc arms route exactly two states —
Unreviewed → reviewers (`AdHocReview`), ChangesRequested → master
(`AdHocRevise`). A CONTINUED or FINISHED ad-hoc gate routes to NO
ONE. The wait's watcher DOES wake on the feedback write (the
feedback path is inside the watched agents dir), refolds, derives
nothing, and parks again — so the report's observable is "no item
by design", not a watcher gap.

**Decision: (a), via the snapshot-transition mechanism.** A
persistent item is impossible (master items have no ack surface —
an always-derived "review passed" item would wake forever), but the
codebase already solves exactly this shape: `detect_finished` wakes
ONCE on a transition observed since the wait's `StartupSnapshot`,
master-gated (`finish-does-not-wake-reviewers`). Mirror it:

- `StartupSnapshot` additionally captures the PENDING ad-hoc shas
  at arm — any gate not yet settled (`!is_settled()`, i.e. not
  Continued/Finished). Broader than `is_open()` deliberately:
  master also sleeps through `ContinuedPendingGate` (the
  gate-reviewers' window) and `Blocked`, and a wait armed there
  must still wake when the verdict lands.
- `detect_adhoc_settled(snapshot, current_adhoc)` emits
  `WaitItem::AdHocSettled { sha, gate }` for captured-pending shas
  now Continued or FINISHED — once per parked wait by construction;
  a wait armed after the transition captures the settled state and
  emits nothing.
- Master-only, like Finished (the reviewer made the verdict).
- Wire: human hint + JSON + the stop-hook mirror, so the wake
  reaches every loop (asyncrewake included).

FINISHED verdicts on ad-hoc commits ride the same item (the plan's
side question) — same transition, same audience.
