# simplify-lifecycle-hooks

## Bug

`clank wfw --role master` blocks when there are no active plans
and `~/.clank/hooks.json` exists. The `wfw-master-empty-exit` fix
is bypassed by the `&& !has_hooks` guard at `wfw.rs:268`.

## Root cause

The snapshot-based lifecycle detection was over-engineered and
didn't work in practice — only `review-received` ever fired
(the gate change happens mid-watch). The `has_hooks` guard kept
master in the watch loop to observe `plan-introduced`, but that
event never fired because plan introductions happen between wfw
invocations.

## Fix: work-item hooks, not lifecycle hooks

Replace the snapshot-based lifecycle detection with simple
work-item notifications. Hooks fire based on what wfw returns,
not on state transitions. Rename events to be honest about what
they are:

- **`master-work`** — fires when `WaitItem::Master` is returned.
  Env includes `CLANK_NEXT` (revise/commit/implement/finalize)
  and `CLANK_GATE` (from the PlanView).
- **`reviewer-work`** — fires when `WaitItem::Reviewer` is
  returned.
- **`plan-finalized`** — fires when `WaitItem::Finished` is
  returned. This is a per-invocation notice (fires whenever wfw
  surfaces a finished plan), not a one-shot lifecycle transition.
- **`idle`** — fires when wfw has no items and nothing to wait
  for. Unlike the others, idle captures stdout — non-empty stdout
  becomes a synthetic prompt that wfw emits so the stop-hook can
  continue the agent.

Hooks fire once per wfw invocation, right before items are
printed. No snapshot state, no transition detection.

## Config

Same files (`~/.clank/hooks.json`, `.clank/hooks.json`), new
event names:

```json
{
  "master-work": "say $CLANK_PLAN $CLANK_NEXT",
  "reviewer-work": "say Review $CLANK_PLAN",
  "plan-finalized": "say $CLANK_PLAN finalized",
  "idle": "echo Check .clank/stubs/ for ideas."
}
```

## What goes

- `LifecycleSnapshot` struct and all methods
- `detect_lifecycle_transitions` / `advance_lifecycle_snapshot`
- Lifecycle detection code in the watch loop (the inline refold
  that called detect/advance is replaced by the existing
  `derive_from_state` call)
- The `has_hooks` variable and `&& !has_hooks` guard
- Old event names (`plan-introduced`, `review-received`) from
  `HookEvent` enum — replaced by `master-work`, `reviewer-work`

## Implementation

### `HookEvent` enum (`crates/core/src/vocab.rs`)

```rust
pub enum HookEvent {
    MasterWork,
    ReviewerWork,
    PlanFinalized,
    Idle,
}
```

Serde kebab-case: `master-work`, `reviewer-work`, etc.

### `WaitItem::Master` — add `gate` field (`crates/core/src/wait.rs`)

Add `gate: CommitGateState` to `WaitItem::Master`. The projection
already has this from the `PlanView`; carrying it on the item
means hooks don't need separate view access. Update `master()`
helper and `derive_work` to pass it through.

### `HookFiring` (`crates/cli/src/hook_config.rs`)

```rust
pub struct HookFiring {
    pub event: HookEvent,
    pub plan: PlanKey,
    pub sha: CommitSha,
    pub gate: Option<CommitGateState>,
    pub next: Option<String>,
}
```

Only used for the three work-item events (`master-work`,
`reviewer-work`, `plan-finalized`). `idle` does NOT go through
`HookFiring` — it's a separate path (see below).

### `run_hook` (`crates/cli/src/hook_config.rs`)

Set `CLANK_GATE` and `CLANK_NEXT` from the firing's fields when
present. Stdout discarded (notifications only). Returns nothing.

### `run_idle_hook` (`crates/cli/src/hook_config.rs`)

Separate function for idle — different shape (captures stdout,
no plan/sha context):

```rust
pub fn run_idle_hook(
    repo: &Path,
    config: &HookConfig,
) -> Option<String> { ... }
```

Sets `CLANK_EVENT=idle` and `CLANK_REPO`. Captures stdout. Returns
`Some(prompt)` if non-empty, `None` otherwise.

### `firings_from_items` (`crates/cli/src/cli/wfw.rs`)

```rust
fn firings_from_items(items: &[WaitItem]) -> Vec<HookFiring> { ... }
```

Map each WaitItem to a HookFiring. `WaitItem::Master` now carries
`gate` directly — no PlanView lookup needed. `next` is serialized
from the `MasterNext` variant name.

### `wfw::run` flow

```
1. Initial fold
2. derive_from_state → items
3. If items non-empty:
     fire work-item hooks (master-work / reviewer-work / plan-finalized)
     emit items, return
4. Master + no plans (`plans.is_empty()` only — NOT "no items"):
     fire idle hook via run_idle_hook (separate from HookFiring)
     → if prompt returned, emit as WaitItem::Idle + return
     → else emit empty items, return (master-empty fast-exit)
   Note: master with active plans but no current items (waiting
   on reviewers) must NOT hit this path — it enters the watch
   loop at step 5 so reviewer feedback can wake it.
5. Enter watch loop
6. On each refold tick:
     derive items
     if non-empty:
       fire work-item hooks
       emit items, return
```

The idle hook runs at step 4 — BEFORE the master-empty fast-exit.
If idle produces a prompt, wfw exits with the prompt as a
synthetic item. If not, fast-exit as today.

For non-master roles with no items, wfw enters the watch loop
(step 5) — idle doesn't fire there (would spam on every tick).

### `WaitItem::Idle` (`crates/core/src/wait.rs`)

Add a new variant for the synthetic prompt:

```rust
Idle { prompt: String }
```

The stop-hook adapter's `render_wfw_items` handles it by
forwarding the prompt text as the continuation reason.

## Tests

- `wfw_master_no_plans_exits_immediately_json`: must pass with
  hooks configured (regression fix).
- `wfw_lifecycle_hook_fires_on_plan_introduced`: rename to
  `wfw_hook_fires_reviewer_work`, update hook key + assertions.
- `wfw_hook_failure_does_not_fail_wfw`: update hook key.
- `wfw_master_empty_blocks_when_hooks_configured`: DELETE — the
  whole point is that master-empty no longer blocks with hooks.
- `wfw_repeated_invocations_do_not_refire_hooks`: DELETE —
  hooks now fire per-invocation by design (one notification per
  wfw call when work exists is correct).
- NEW: `wfw_idle_hook_returns_prompt` — master + no plans + idle
  hook configured → wfw exits 0 with the prompt as a synthetic
  item.
- NEW: `wfw_master_no_plans_no_idle_exits_empty` — master + no
  plans + no idle hook → fast-exit with empty items.

## Acceptance criteria

- Master + no plans → fast-exit regardless of hooks config
  (unless idle hook produces a prompt).
- Work-item hooks fire for every item wfw returns.
- `idle` hook captures stdout; non-empty stdout becomes a
  synthetic `WaitItem::Idle` prompt.
- No `LifecycleSnapshot` or snapshot comparison code remains.
- `HookEvent` uses the new names (`master-work`, `reviewer-work`,
  `plan-finalized`, `idle`).
