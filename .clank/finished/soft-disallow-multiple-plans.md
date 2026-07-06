# soft-disallow-multiple-plans
# Soft-disallow multiple open plans: a master warning item at HEAD

## Why

Clank tolerates N active plans, but interleaved plan work is where every
history-edit primitive gets weaker: stash refuses interleaved ranges,
squash needs bounded spans, reviews split attention. The queue exists so
work WAITS instead of overlapping. When a master nonetheless commits work
for a second plan while one is open, clank should say so immediately —
at HEAD, while the fix is still a clean stash — with concrete remedies.

## Investigation facts (verified in code, current master)

- **`stash push` CANNOT take a buried plan.** `build_rewrite_preview`
  ranges `intro(plan)..HEAD` (preview.rs:150-171); every unattributed
  commit in that range is `foreign`, and `safety_check` refuses ANY
  foreign unconditionally — `--force` does not bypass (stash.rs:563-574).
  So with old plan A under new plan B's commits, `stash push A` refuses;
  only the plan whose commits form the tip suffix is stashable. Remedy
  wording MUST order the stashes: new first, then old, then pop new.
- The precedent for "warning actions for master" is
  `WaitItem::FixCommitTag` (core wait.rs:104, produced from
  `WorkStatus::head_correction`): HEAD-only, adoption-gated, computed
  once in `derive_status`, and in `work_for` (wait.rs:968-985) it
  preempts the master's items for the round.
- Queue promotion is only offered when `actionable == 0` — no
  unsuppressed active plan (cli wait.rs:249-255). BUT a **blocked**
  active plan is suppressed, so clank itself invites promoting a second
  plan alongside a blocked one. The new warning must agree with that
  gate or wait contradicts its own advice.

## What

A new master-routed wait item, sibling of `FixCommitTag`:

```rust
WaitItem::MultiplePlansOpen {
    /// Oldest open plan by intro position — "the plan in progress".
    in_progress: PlanKey,
    /// Every other open plan, intro order — "the new plan(s)".
    new_plans: Vec<PlanKey>,
    sha: CommitSha,   // HEAD, for display parity with FixCommitTag
}
```

**Trigger — current state at HEAD, never history:** fires when the fold
at HEAD has ≥ 2 open plans that are NOT block-suppressed, on an adopted
repo. It does not scan for past periods of overlap (same philosophy as
the removed prefix-ambiguity warnings: history is tolerated, mistakes
are caught live at HEAD). It keeps firing every wait round until the
state resolves, and clears the moment only one non-blocked plan remains.

**Blocked plans don't count.** If plan A is blocked and clank's own
promotion gate suggested starting B, warning about A+B would fight
wait's own advice. The count uses the same notion of "actionable plan"
as the promotion gate. When A unblocks with B still open, the warning
fires — correctly: that's the moment the overlap becomes real.

**Precedence:** `head_correction` (broken tag) still preempts
everything. Then this warning preempts the master's other items — the
master's next action IS resolving the overlap; all three remedies go
through it. **Reviewers are NOT silenced** (unlike FixCommitTag): this
is the soft in soft-disallow — review flow on whatever the gate wants
reviewed continues; stash/pop resets reviews anyway (new shas), and
remedy 3 continues the existing gate untouched.

**Message** (human line in cli wait.rs `render`, mirrored in the JSON
item; stop-hook nudge inherits it):

> There is already a plan `<in_progress>` in progress, but there are
> commits for `<new>` before `<in_progress>` has been finished. Resolve
> by ONE of:
>   1. Park `<in_progress>`, continue `<new>`:
>      `clank stash push <new>`, then
>      `clank stash push <in_progress> --for <new>`, then
>      `clank stash pop <new>`. (Stash only takes the plan whose commits
>      sit at the tip, so the new one goes first.)
>   2. Finish `<in_progress>` first: `clank stash push <new>`, reduce
>      `<in_progress>`'s scope if needed, finish it, then
>      `clank stash pop <new>`.
>   3. Fold `<new>` into `<in_progress>`: drop `<new>`
>      (`clank purge --drop <new>`), roll its work into
>      `<in_progress>`, widening its scope.

With >2 open plans, name all new plans and keep the same remedies
per-plan (rare; text may list them comma-separated).

Note the option-1 ordering is a hard fact of today's stash, not style:
stash new → stash old → pop new. The `--for` on the old plan feeds the
existing "FINISHED; pop?" readiness nudge.

## Where

- `crates/core/src/wait.rs`: compute in `derive_status` next to
  `head_correction` (fold order gives intro positions; blocked
  detection already exists for `WaitingOn::Blocked`); store on
  `WorkStatus`; route in `work_for` after the head-correction preempt.
  Pure — unit-testable without git.
- `crates/cli/src/cli/wait.rs`: `WaitJsonItem` variant
  (`kind: "multiple_plans_open"`) + human render with the numbered
  remedies.
- No new CLI commands; no per-plan `WaitingOn` change (plan rows keep
  their real state — reviewers still flow).

## Out of scope (candidate follow-up queue item)

Teaching `stash push` to take a buried plan (span-bounded foreign check
+ restack of descendants, exactly the buried-squash model) would let
remedy 1 collapse to `clank stash push <in_progress> --for <new>` alone.
Deliberately not here — the warning must land with remedies that work
against TODAY's stash.

## Tests (pure, core wait.rs; JSON/human-line tests in cli wait.rs)

- Two open plans → master gets exactly `MultiplePlansOpen`
  (in_progress = older by intro, new_plans = [newer]); reviewer items
  unchanged.
- One open plan → absent. Three open → both newer plans named, intro
  order.
- Broken HEAD tag + two plans → `FixCommitTag` only.
- Older plan blocked + newer active → absent (agrees with the
  promotion gate). Unblock with both open → fires.
- Not adopted → absent.
- Human line names both plans and all three remedies with the
  stash-new-first order (model:
  `fix_commit_tag_human_line_names_all_three_kinds`).

## Acceptance

Master committing `[new] intro` while `old` is open sees the warning on
the next wait/stop-hook round with the three remedies; it clears after
any remedy; reviewers were never stalled by it; clippy/fmt/suites green.
