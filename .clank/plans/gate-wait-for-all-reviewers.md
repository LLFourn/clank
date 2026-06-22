# gate-wait-for-all-reviewers

The master is woken to revise on the FIRST commit-reviewer Request-Changes,
before the other reviewers have verdicted — so a second reviewer never gets
to weigh in on that commit (they end up reviewing the master's revision
instead, and the master may revise on partial feedback). Root cause + a
centralization gap, both in `crates/core/src/wait.rs`.

## The bug

`compute_gate` (wait.rs:459-466) returns `ChangesRequested` if ANY submitted
reviewer voted Request-Changes/Unmarked, regardless of whether the OTHER
expected reviewers have verdicted. `reviews` is filtered to expected
reviewers who have SUBMITTED, so a silent reviewer isn't in the set — one
Request-Changes among pending reviewers flips the gate to `ChangesRequested`
→ `waiting_on = MasterToRevise` (wait.rs:677-692) → master woken. There is
no "all reviewers done?" check on this path (`missing_for_gate` returns
empty for `ChangesRequested`, wait.rs:855-859).

Expected behavior: while any expected reviewer of the active tier is still
pending, the gate stays in a reviewers-wake / master-sleeps posture and the
pending Request-Changes is HELD. Only when ALL expected reviewers have
verdicted AND at least one requested changes → `ChangesRequested` → master
woken with the full feedback set.

## The centralization gap (the deeper issue — and why the bug class exists)

The gate STATE computation is already single-source: `compute_gate` is the
one function (`lib.rs:32-33` documents this; `preview.rs:478` and
`pr_review.rs:440` delegate to it; the status TUI consumes the derived
`waiting_on` and does not recompute). So there are NO duplicated
implementations of the gate state — confirmed.

BUT the bug exists because a key state-machine question — "have all
expected reviewers of the active tier verdicted?" — is IMPLICIT (woven into
the gate-state transitions) rather than a centralized, named predicate. That
is why the wait-for-all check is absent on the `ChangesRequested` path.
Centralizing that question is what prevents the whole class of "acted on
partial reviews" bugs — not patching this one arm.

## Approach

1. Add a single-source review-coverage summary for the active tier (expected
   / submitted / pending / positive / changes), computed ONCE. `compute_gate`,
   the `waiting_on` derive (wait.rs:665-754), and `missing_for_gate`
   (wait.rs:844) all consume it — no re-derivation across branches.
2. Restructure `compute_gate` so `ChangesRequested` (a master-turn state) is
   reached ONLY when all expected reviewers of the active tier have
   submitted. A Request-Changes while reviewers are still pending →
   reviewers-wake / master-sleeps (the pending reviewer is in the missing
   set; the held Request-Changes is carried so the master sees the full set
   once the last verdict lands).
3. Preserve the existing milestone/tier semantics (gate tier consulted only
   at milestones; the empty-commit-tier devolve rule at wait.rs:423-427,
   499-504) — the coverage summary must respect them so routine WIP commits
   still bypass the gate tier straight to `Approved`.

## Testing (no-binary-spawning)

- Regression: TWO commit reviewers — A requests changes, B silent → master
  SLEEPS, B wakes (not the master). A requests + B approves → master wakes
  (`MasterToRevise`) with both. A requests + B requests → master wakes with
  both. Single reviewer requests → master wakes (no regression of the
  common case).
- Coverage-summary unit tests: expected/submitted/pending/positive/changes
  partition; gate-tier milestone conditioning (routine commit → gate tier
  not consulted).
- Existing `compute_gate` / `waiting_on` tests stay green for the
  all-verdicted cases (Unreviewed / ApprovedPendingGate / Approved / Finished
  paths unchanged when everyone has spoken).

## Acceptance

- A Request-Changes from one reviewer while another is pending does NOT wake
  the master; the pending reviewer wakes. The master wakes to revise only
  once all expected reviewers of the active tier have verdicted.
- "All expected reviewers verdicted?" is a single, named, consumed-
  everywhere summary — no implicit re-derivation across gate branches.
- Existing tests green; clippy within budget (core); fmt clean.

## Out of scope

- The gate-tier wake timing itself (ApprovedPendingGate) — unchanged; this
  plan is about the commit-tier "act only on a complete review" rule.
- Any UI/TUI rewording of `ChangesRequested` display — the state name stays;
  only its reachability condition changes.
