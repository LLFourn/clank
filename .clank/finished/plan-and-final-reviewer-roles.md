# plan-and-final-reviewer-roles
# Split the gate tier into `plan` and `final` reviewer roles

## Problem

A reviewer today is `commit` (reviews every commit) or `gate`. The
`gate` tier is consulted at TWO distinct milestones, already
computed in `compute_gate` (`core/wait.rs:481-501`):

1. **plan milestone** — the latest reviewable commit changed a plan
   doc (`latest_touched_plan`) and the commit tier continued it.
2. **finish milestone** — the commit tier marked the commit FINISHED
   (`commit_finished`).

We want those two to be SEPARATELY assignable, so a reviewer can do
one without the other:

- **`plan`** — reviews ONLY plan-doc commits (the plan milestone),
  after the commit reviewers have all continued. Never woken at the
  finish.
- **`final`** — reviews ONLY at the end (the finish milestone), after
  the commit reviewers have all FINISHED. Never woken on a plan
  commit.
- **`gate`** — both (today's behavior). `gate` IMPLIES `plan` + `final`.
- **`commit`** — unchanged (every commit).

(Role name `final` chosen over `finished` to avoid clashing with the
FINISHED verdict / `clank finish`.)

## Key insight: the state machine doesn't change

`CommitGateState` (`Unreviewed` / `ContinuedPendingGate` /
`ChangesRequested` / `Continued` / `Finished`) is UNCHANGED. The two
milestones already exist. The ONLY change is WHICH second-tier
reviewers are consulted at each milestone:

- plan milestone → the **plan tier** = reviewers with role `plan` OR
  `gate`.
- finish milestone → the **final tier** = reviewers with role `final`
  OR `gate`.
- a commit that is BOTH (a plan-doc commit the commit tier also marked
  FINISHED — rare) → both tiers active (their union).
- the `Finished` vs `Continued` decision still belongs to the FINISH
  path: `Finished` iff commit tier all-finished AND the **final tier**
  all-finished. Plan-only reviewers never block the finish; final-only
  reviewers never block a plan commit.

## Backward compatibility

Non-breaking. `gate` stays a role and keeps meaning "plan + final", so
every existing `~/.clank` / repo config with `role: gate` behaves
exactly as before. `plan` / `final` are additive opt-in roles.

## Design

### Roles & vocab
- `RosterRole` (`teams_config.rs`): add `Plan`, `Final` (keep
  `Master`, `Commit`, `Gate`). snake_case serde (`"plan"`, `"final"`).
- `ReviewKind` + `ReviewKindArg` (`cli/mod.rs`): add `Plan`, `Final`
  so `clank agent add <x> --review plan|final|gate|commit` and
  `clank agent promote`/`set-review` accept them. `From<ReviewKind>
  for RosterRole`.

### Resolution → tiers

`RegisteredSet` stores reviewers as ONE role-tagged collection, not N
role-keyed buckets — `reviewers: Vec<RegisteredReviewer{label, desc,
role}>`. This is the architectural fix for the enumeration fragility
the role split exposed (codex 0976211): with per-role buckets, every
"enumerate all reviewers" site hand-chained `commit + gate` and
silently dropped plan/final — a logic omission the compiler can't
catch (status roster rows, open_zellij panes, fork session-bind,
`clank agent` listing all had it). With one collection:

- "every reviewer" is `set.reviewers` — there is nothing else to
  chain, so dropping a role is unrepresentable.
- the role→tier mapping (incl. the `gate = plan + final` fold) lives
  in ONE place: `RegisteredSet::commit_tier()` / `plan_tier()`
  (Plan|Gate) / `final_tier()` (Final|Gate), each a `match`/filter on
  `role`. Adding a future role makes the compiler flag that one match.
- display surfaces iterate `reviewers` and read `r.role` (a `gate`
  reviewer is still listed ONCE).

- `resolve_registered_set` (`teams_config.rs`) pushes every reviewer
  into the single `reviewers` Vec with its role.
- `WorkPolicy` (`core/wait.rs`): replace `gate_reviewers` with
  `plan_reviewers` + `final_reviewers` (each already includes gate),
  built from the tier accessors via `agent_store::ReviewerTiers`.
- Convert ALL reviewer enumerators (status `roster_auto_rows`,
  `fork`, `open_zellij` ×2, `doctor`, `clank agent` listing,
  `role_from_registered_set`) to iterate `reviewers`.

### Gate computation
- `compute_gate(reviews, commit_reviewers, plan_reviewers,
  final_reviewers, latest_touched_plan)`:
  - commit tier unchanged.
  - active second tier = (plan milestone ? plan_reviewers : ∅) ∪
    (finish milestone ? final_reviewers : ∅); `is_milestone` =
    either. Non-milestone → `Continued`.
  - tier coverage on the active set → `ContinuedPendingGate` /
    `ChangesRequested` as today.
  - `Finished` iff `commit_finished` AND final_tier all-finished
    (plan-only reviewers excluded from the finish decision).
- `missing_for_gate` / `work_for`: at a plan milestone wake the
  pending plan_reviewers; at a finish milestone wake the pending
  final_reviewers (their union when both).

### Devolve-guard substitutions (preserve today's edge behavior)

The split is NOT a single-field swap of `gate_reviewers`. Two existing
guards must use the right set (ruthless 36ba302):

- **No-second-tier early return** (`wait.rs:443`, today
  `commit.is_empty() && gate.is_empty()` → `Continued`): becomes
  `commit.is_empty() && plan_reviewers.is_empty() &&
  final_reviewers.is_empty()` — the UNION of both new tiers empty.
- **`Finished` decision** (`wait.rs:520`, today `commit all-finished
  && gate all-finished`): becomes `commit all-finished && FINAL-tier
  all-finished`. Plan-only reviewers are excluded from the finish.
- **Devolve guard stays on the commit tier**: `commit_finished`
  already requires a NON-EMPTY commit tier (an empty commit tier makes
  `all(finished)` vacuously true and would re-animate the regression).
  Consequence to call out: the FINISH milestone only fires when there
  IS a commit tier to signal FINISHED, so a `final` reviewer depends on
  a commit tier — a final-only repo with no commit reviewer reaches
  only the plan milestone (via `latest_touched_plan`), never the
  finish. Consistent with the existing guard; kept.

### Surfaces to update (all current two-tier consumers)
- `compute_gate` callers: `derive_status` (status), `preview.rs`,
  `cli/pr_review.rs`.
- `RosterJson` / `team show`/`list` (`cli/team.rs`): add `plan` /
  `final` buckets to the `--json` + human output (alongside
  `commit_reviewers` / `gate_reviewers`).
- Status TUI agent panel role labels + `doctor.rs` reviewer listing.
- `AgentAutoRow.role` is already `RosterRole`, so the panel renders
  the new variants once they exist.

## Acceptance criteria

- Roles `plan` / `final` added; `gate` kept = plan + final; CLI
  `--review plan|final|gate|commit` works.
- `RegisteredSet` stores ONE role-tagged `reviewers` collection (not
  per-role buckets); every enumerator (status panel, fork, zellij ×2,
  doctor, `clank agent` list, console roster) iterates it, so no role
  can be silently dropped. Regression test: plan/final reviewers
  appear in the roster enumeration.
- `compute_gate` tests:
  - a `plan` reviewer is woken at a plan-doc-commit milestone and is
    NOT consulted at a finish milestone;
  - a `final` reviewer is woken at a finish milestone and is NOT
    consulted on a plan-doc commit;
  - a `gate` reviewer is woken at BOTH (regression: identical to
    today's gate behavior);
  - `Finished` requires the final tier (not plan-only reviewers) all
    finished; a plan-only reviewer left at Continue does NOT block the
    finish.
  - **Empty-commit-tier guard**: a plan-doc commit (`latest_touched_plan`)
    with a `final` reviewer already at FINISHED but NO commit tier is
    NOT marked `Finished` (the finish milestone needs a non-empty
    commit tier; `all(finished)` must not devolve to `Finished` via
    vacuous truth on the empty commit set). The no-second-tier early
    return uses the UNION of plan + final being empty.
- Backward-compat test: an existing `role: gate` roster resolves and
  gates exactly as before (plan + final).
- `work_for` wakes the right tier per milestone (plan vs final),
  pinned by a test.
- `team show`/`--json`, doctor, and the TUI panel show the new roles.
- No change to `CommitGateState` or the master/commit paths.

## Out of scope

- The `head_correction` preempt and the shared watcher (just shipped).
- Any reordering of milestones or new gate states — this only splits
  WHICH reviewers are consulted at the existing two milestones.
- PR-review gate semantics beyond passing the new tiers through
  `compute_gate` (the PR path reuses it; keep its behavior).
