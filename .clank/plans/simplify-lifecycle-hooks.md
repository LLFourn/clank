# simplify-lifecycle-hooks

## Bug

`clank wfw --role master` blocks when there are no active plans
and `~/.clank/hooks.json` exists. The `wfw-master-empty-exit` fix
is bypassed by the `&& !has_hooks` guard at `wfw.rs:268`, which
was added so the watch loop could observe `plan-introduced` via
snapshot-based lifecycle detection.

The snapshot approach turned out to be over-engineered: hooks only
fire for transitions during a wfw watch session, never for events
between invocations. In practice the only hook that ever fired was
`review-received` (the gate change happens mid-watch). Plan-
introduced and plan-finalized never fired because they happen via
commits that complete before wfw starts.

## Fix: fire hooks from work items, not snapshots

Replace the snapshot-based lifecycle detection with a simple
mapping from `WaitItem` variants to hook events. Fire hooks right
before wfw prints items:

- `WaitItem::Reviewer` → fire `plan-introduced` hook
- `WaitItem::Master` → fire `review-received` hook
- `WaitItem::Finished` → fire `plan-finalized` hook

Pass the item's plan, sha, and (for review-received) the gate
state as env vars.

This covers all cases because wfw always produces items before
exiting. No snapshots, no transition detection, no empty-vs-
capture lifecycle state. The work items ARE the lifecycle events.

## What goes

- `LifecycleSnapshot` struct and `capture`/`empty` constructors
- `detect_lifecycle_transitions` function
- `advance_lifecycle_snapshot` function
- The `has_hooks` variable and the `&& !has_hooks` guard on the
  master-empty early-exit
- All snapshot-related code in the watch loop
- The `HookEvent` enum in `crates/core/src/vocab.rs` (the event
  name is derived from the `WaitItem` variant, not from a
  separate enum — or keep the enum if the hook config keys still
  need `plan-introduced`/`review-received`/`plan-finalized`)

## What stays

- `HookConfig` type and `load_hook_config` / `merge_from_file`
- `HookFiring` struct and `run_hook`
- `HookEvent` enum (for config key deserialization)
- `~/.clank/hooks.json` and `.clank/hooks.json` config loading

## Implementation

In `wfw.rs`, replace the lifecycle detection code with a simple
function that maps items to firings:

```rust
fn firings_from_items(items: &[WaitItem]) -> Vec<HookFiring> {
    items.iter().map(|item| match item {
        WaitItem::Reviewer { plan, sha, .. } => HookFiring {
            event: HookEvent::PlanIntroduced,
            plan: plan.clone(),
            sha: sha.clone(),
            gate: None,
        },
        WaitItem::Master { plan, sha, .. } => HookFiring {
            event: HookEvent::ReviewReceived,
            plan: plan.clone(),
            sha: sha.clone(),
            gate: /* from the PlanView or derive */,
        },
        WaitItem::Finished { plan, finalized_at } => HookFiring {
            event: HookEvent::PlanFinalized,
            plan: plan.clone(),
            sha: finalized_at.clone(),
            gate: None,
        },
    }).collect()
}
```

Call it before `emit` on both the initial-fold and watch-loop
paths. Remove the `has_hooks` guard from the master-empty
early-exit.

## Tests

- **Regression**: `wfw_master_no_plans_exits_immediately_json`
  must pass even with hooks configured. Add a variant that writes
  a hooks.json first.
- **Existing hook tests**: `wfw_lifecycle_hook_fires_on_plan_
  introduced` and `wfw_hook_failure_does_not_fail_wfw` still pass
  (they use the watch-loop path which produces items).
- **Repeated invocations**: `wfw_repeated_invocations_do_not_
  refire_hooks` — this test may need updating since hooks now
  fire on items (which ARE produced on repeated invocations for
  pre-existing reviewer work). The test's intent (no spam) needs
  rethinking: hooks fire once per wfw invocation if work exists,
  which is correct — each invocation is a separate notification.

## New event: `idle`

A fourth hook event. Fires when wfw has checked for work and
found nothing — right before it would block (watch loop) or exit
empty (master fast-exit).

Unlike the other hooks, `idle` captures stdout. If the hook
writes non-empty stdout, wfw treats it as a synthetic prompt and
emits it as a special work item so the stop-hook can use it as a
continuation prompt. If stdout is empty (or the hook isn't
configured), wfw proceeds as today (block or fast-exit).

Use case: the hook command can inspect the repo and suggest what
to work on next — check stubs, run tests, triage issues. The
agent gets a continuation prompt instead of ending its turn.

Config:

```json
{
  "idle": "echo 'No plans in flight. Check .clank/stubs/ for ideas.'"
}
```

The idle hook runs **once per wfw invocation** on the initial
fold's empty-items path. It does NOT run on every watch-loop
tick — that would spam. For the watch loop, wfw blocks as normal
waiting for FS events.

### Implementation

In `wfw::run`, after `derive_from_state` returns `None` on the
initial fold (and after the master-empty fast-exit check):

```rust
if let Some(idle_cmd) = hook_config.get(&HookEvent::Idle) {
    let output = Command::new("sh")
        .arg("-c").arg(idle_cmd)
        .env("CLANK_EVENT", "idle")
        .env("CLANK_REPO", repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    let prompt = String::from_utf8_lossy(&output.stdout)
        .trim().to_string();
    if !prompt.is_empty() {
        // Emit as a synthetic item so the stop-hook picks it up
        emit_idle_prompt(&prompt, args.json);
        return Ok(());
    }
}
// fall through to watch loop or fast-exit
```

The `emit_idle_prompt` function emits a JSON envelope (in
`--json` mode) or human text that the stop-hook adapter can parse
and forward as a continuation prompt.

### WaitItem extension

Add `WaitItem::Idle { prompt: String }` to represent the
synthetic work item. The stop-hook adapter's `render_wfw_items`
handles it by forwarding the prompt text directly.

## Acceptance criteria

- Master + no plans + hooks configured → exits immediately
  (exit 0, empty items) unless an `idle` hook produces a prompt.
- Hooks fire for every work item wfw returns, on both the
  initial-fold and watch-loop paths.
- `idle` hook captures stdout; non-empty stdout becomes a
  synthetic prompt that the stop-hook forwards as a continuation.
- No `LifecycleSnapshot` or snapshot comparison code remains.
- All existing wfw tests pass.
