# wfw-master-empty-exit

## Bug

`clank wfw --role master` blocks until timeout when the repo has
zero active plans. With `clank auto on --mode wait` (indefinite
timeout) the Stop hook hangs forever waiting for a plan that may
never appear, deadlocking the agent's turn.

Reproduction:

```sh
clank wfw --role master --timeout 5s
# blocks 5s, exits 2 (wfw timed out)
```

Expected: returns immediately. There is no review for master to
wait on; if master wanted clank work they would be writing a plan,
not blocking in wfw.

## The asymmetry wfw doesn't model

`clank wfw` today treats "no work from `derive_work`" identically
for both roles: drop into the FS-watch loop and refold on every
event until something changes or timeout expires. That collapses
two very different role surfaces:

- **Reviewer**: reactive. Their work appears when *someone else*
  commits a reviewable change. An empty active-plan set is not a
  terminal state — a plan can land any moment and the reviewer
  should be ready. Blocking is correct.
- **Master**: proactive. Their work appears when *reviewers vote*
  on a commit master already made. If no plans exist, master has
  nothing committed for anyone to vote on. There is no chain of
  events anyone but master can trigger that produces master work.
  Blocking is wrong — master should exit and go write something.

The fix encodes that asymmetry at the wfw boundary so every caller
(humans, scripts, the wait-mode Stop hook) sees consistent
behavior.

## Scope: exactly one new exit path

In `crates/cli/src/cli/wfw.rs`, between the initial
`derive_from_state` call (line ~127) and the FS-watch setup (line
~141), add one branch:

```rust
if role == Role::Master && initial_state.fold.plans.is_empty() {
    emit(&[], args.json);
    return Ok(());
}
```

That's the whole behavior change. The rest of the diff is tests
and documentation.

### What this is NOT

- **Not** a change for `Role::Reviewers`. Reviewers continue to
  block on an empty plan set — correct for their wait surface.
- **Not** a change when master has active plans but no current
  work item (i.e., master is waiting on reviewers). Reviewer
  feedback files arriving will transition the gate and produce
  master work; blocking is the whole point of the wait.
- **Not** a change for the `--plan <name>` filter. With an
  explicit plan filter, the early-exit guards at lines 95-119
  already handle "plan doesn't exist" (error) and "plan finished"
  (emit Finished + exit). The new branch only fires when no
  filter is set AND the unfiltered active-plan set is empty.
- **Not** a change to exit codes. Exit 0 with an empty items
  envelope, same as a successful run. The wait-mode Stop hook
  already treats `Some(0) + items.is_empty()` as `Silent`
  (`stop_hook.rs:206`), so no hook-side change is required.

## Why the fix lives in wfw, not the Stop hook

The wait-mode hook adapter (`stop_hook.rs:156-225`) spawns wfw as
a subprocess and parses its output. We could mirror the hint
mode's "branch 3: nothing pending" check there as a pre-flight
before spawning wfw. That works for the hook path but leaves
`clank wfw --role master` itself broken for every other caller
(human at a terminal, future scripts, future agent integrations
that don't go through `clank stop-hook`).

The bug is in the primitive. Fix it in the primitive.

## Implementation

### `crates/cli/src/cli/wfw.rs`

After `derive_from_state` returns `None` (line ~136), before
building the WatchContext, insert the role-aware early-exit:

```rust
if let Some(items) = derive_from_state(...).await? {
    emit(&items, args.json);
    return Ok(());
}

// No items from the initial fold. For Role::Master with no
// active plans there is structurally nothing that can produce
// master work without master committing something first — exit
// immediately rather than blocking on a watcher that has
// nothing to watch for.
if role == Role::Master && plan_filter.is_none() && initial_state.fold.plans.is_empty() {
    emit(&[], args.json);
    return Ok(());
}

let watch_ctx = WatchContext::resolve(&repo, poll_mode)?;
// … existing loop …
```

The `plan_filter.is_none()` guard is defensive — `--plan <name>`
on an active plan already establishes "this exists," and the
early-exit guards at lines 95-119 already handle finished /
missing plans. Including the check makes the precondition
explicit at the new branch.

### `emit(&[], json)` for empty items

`emit` is currently called only with non-empty items. The empty
case needs to produce parseable output so the wait-mode hook's
`parse_wfw_json` doesn't trip on empty stdout:

- JSON mode: emit `{"items": []}` (same shape, just empty array).
  `emit` already builds the envelope generically; passing an
  empty slice produces `{"items": []}` with no code change.
- Human mode: emit nothing. The current `for item in items`
  loop iterates zero times. Consider a one-line stderr hint for
  interactive humans: `no work; no active plans for master`.
  Optional; keep the JSON path strict and the human path
  informative.

Verify by re-reading `emit` — no code change should be needed
beyond calling it with `&[]`.

## Tests

Add to `crates/cli/tests/wfw_integration.rs` (or the existing
file with the closest neighbor):

1. **Master, no plans, JSON mode**: `clank wfw --role master
   --json --timeout 30s` in a clank-initialized repo with no
   plans. Asserts:
   - exit code 0
   - stdout is `{"items":[]}\n` (or equivalent parsed shape)
   - completes in <1s (not waiting near the timeout)

2. **Master, no plans, human mode**: same setup, no `--json`.
   Asserts exit 0, completes fast, stdout shape per the chosen
   human-mode rendering.

3. **Reviewer, no plans (regression guard)**: `clank wfw --role
   reviewers --author alice --timeout 2s` with no plans. Asserts
   it times out (exit 2). This locks in the asymmetry — if a
   future change accidentally widens the early-exit to both
   roles, this test catches it.

4. **Master, plan exists but waiting on reviewers (regression
   guard)**: master committed a plan; no feedback yet. wfw
   --role master --timeout 2s should still block (and time out).
   Locks in "master blocks when there's something to wait for."

5. **Master, `--plan foo` on an active plan with no master work**:
   should still block. Locks in the `plan_filter.is_none()`
   guard.

## Acceptance criteria

- `clank wfw --role master` with zero active plans returns
  immediately with exit 0 and an empty items envelope.
- `clank wfw --role reviewers` behavior is unchanged.
- `clank wfw --role master` with active plans (any state) is
  unchanged — still blocks until work or timeout.
- The wait-mode Stop hook no longer hangs the agent's turn when
  there are no plans in the repo.
- New integration tests lock in both the fixed behavior and the
  reviewer asymmetry as a regression guard.

## Out of scope

- Whether master should ever block on reviewer activity for an
  in-flight plan. That's a workflow design question — the
  current behavior (block while plan is in flight) is preserved.
- Hint mode. The hint hook already handles this case correctly
  via its "branch 3: nothing pending → Silent" path
  (`stop_hook.rs:144-146`).
- The `--plan <name>` finished-plan path. Already handled.
- Multi-agent / multi-machine scenarios where a plan could
  appear via an external commit. If that becomes a real
  workflow, master could opt into the reviewer-style blocking
  with an explicit flag — out of scope for this bug fix.
