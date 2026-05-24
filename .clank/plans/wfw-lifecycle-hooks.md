# wfw-lifecycle-hooks

## Summary

Add configurable shell hooks that `clank wfw` executes when it
detects a plan lifecycle transition. Three events:

- **`plan-introduced`** — a new plan appeared in the fold.
- **`review-received`** — a reviewer posted feedback that changed
  the gate state on a plan.
- **`plan-finalized`** — `clank finish` completed; plan moved to
  `finished/`.

Hooks run **blocking, inside `wfw`**, before wfw prints its items
and exits. This means one `clank wfw` call does detect + notify —
the calling agent (or human) doesn't need a separate watcher.

## Why

The Stop hook works well for agents, but there's no notification
path for the *other* side of a transition. When codex approves a
commit, master's wfw detects the state change and returns work —
but there's no way to run a command (e.g. desktop notification,
webhook, sound) at the moment the transition is detected. Agents
want this for the same reason humans do: "something changed,
react."

## Configuration

Two config files, merged (repo overlays user defaults):

1. **`~/.clank/hooks.json`** (user-level, global defaults)
2. **`.clank/hooks.json`** (repo-level, gitignored, overrides
   user per-event)

Schema:

```json
{
  "plan-introduced": "notify-send 'New plan: $CLANK_PLAN'",
  "review-received": "notify-send 'Review on $CLANK_PLAN'",
  "plan-finalized": "notify-send 'Finalized: $CLANK_PLAN'"
}
```

Each value is a shell command string executed via `sh -c`. Absent
keys mean no hook for that event — omit the key to disable.

Merging: load user-level defaults first, then overlay repo-level
entries. Repo wins for any event defined in both. Events only in
the user config are preserved as defaults.

## Event detection

Hooks fire on **lifecycle state transitions**, not on work items.
This makes them role-independent — a master's wfw and a reviewer's
wfw both detect the same transitions and fire the same hooks.
Work items (WaitItem::Master/Reviewer/Finished) are role-filtered
and printed as today; hooks are separate.

Detection uses a snapshot-comparison approach similar to
`detect_finished`:

### `plan-introduced`

Track a `BTreeSet<PlanKey>` of known active plans at startup.
On each refold, if a new plan key appears in `state.fold.plans`
that wasn't in the set, fire the hook and add it to the set.

### `review-received`

Track per-plan `(latest_reviewable_sha, gate_state)` at startup.
Track per-plan `(latest_reviewable_sha, gate_state)` pairs across
refold ticks. Fire `review-received` only when the gate state
changes **on the same SHA** — that means reviewer feedback landed,
not a new commit.

When the SHA changes (master committed a revision), the gate
commonly resets to `Unreviewed`. That's a master-driven event, not
a review. The SHA-pinning filter prevents a false fire:

- Same SHA + gate `Unreviewed → Approved`: reviewer approved → fire.
- Same SHA + gate `Unreviewed → ChangesRequested`: reviewer
  requested changes → fire.
- Different SHA + gate `Approved → Unreviewed`: master committed
  a new revision, gate reset → don't fire.

This avoids both the `waiting_on` problems (worktree dirtiness,
master-driven resets) and the pure `gate_state` problem (SHA
movement resets the gate independently of feedback).

### `plan-finalized`

Track `finished_shas: BTreeMap<PlanKey, BTreeSet<CommitSha>>` in
the lifecycle snapshot (same shape as `StartupSnapshot`'s
`finished_at_startup`). On each refold, compare current
`state.fold.finished_plans` against the tracked set. New entries
fire `plan-finalized`. Update the tracked set unconditionally.

Do NOT piggyback on `detect_finished` / `StartupSnapshot` — its
`watched` set is fixed at startup and doesn't include plans
introduced mid-session. The lifecycle snapshot maintains its own
finished-plan tracking so a plan introduced and finalized during
the same wfw invocation is correctly observed.

## Environment variables

Hook commands receive context via env vars:

| Variable | Description | All events |
| --- | --- | --- |
| `CLANK_EVENT` | Event name (`plan-introduced`, etc.) | yes |
| `CLANK_PLAN` | Plan stem (e.g. `my-feature`) | yes |
| `CLANK_REPO` | Absolute repo path | yes |
| `CLANK_SHA` | Relevant commit SHA | yes |

## Hook execution

Lifecycle detection is a **separate path from work-item
derivation**. On every refold tick (initial fold and each watch-
loop iteration):

1. Project plan views.
2. Detect lifecycle transitions by comparing current state to the
   lifecycle snapshot. For each transition matching a configured
   hook, run the hook command via `sh -c` with env vars set.
   Block until the hook exits. Hook stdout is captured and
   discarded (never inherited — `wfw --json` stdout must stay
   clean for the stop-hook parser). Hook stderr is forwarded to
   wfw's stderr. If the hook exits non-zero, log a warning but
   don't fail wfw — hooks are best-effort notifications, not
   gates.
3. **Advance the lifecycle snapshot unconditionally** — update
   `known_plans`, `review_state`, and `finished_shas` to the
   current state regardless of whether any event had a configured
   hook. This ensures later transitions compare against fresh
   state even for events the operator chose not to hook.
3. Derive work items for this role (existing `derive_work` +
   `detect_finished`).
4. If items non-empty, print and exit.
5. Otherwise, loop back to the next refold tick.

This ordering means hooks fire on transitions that don't produce
role-filtered work items (e.g. `plan-introduced` fires for a
master agent even though the intro commit produces `Reviewer`
work, not `Master` work).

### Master-empty early-exit interaction

The `wfw-master-empty-exit` plan added an early return when
`role == Master` and the active plan set is empty — master has
nothing to wait for. That early-exit skips the watch loop, so
master's wfw can't observe `plan-introduced` for a plan committed
after wfw starts.

When hooks are configured: **skip the master-empty early-exit**
and enter the watch loop. The hook system needs the watcher to
observe new plans. Work items still won't appear for master (no
plans = no master work), but hooks will fire.

When no hooks are configured: preserve the early-exit (existing
behavior, no regression).

## Implementation surface

### `crates/core` — new types

- `HookEvent` enum: `PlanIntroduced`, `ReviewReceived`,
  `PlanFinalized`. Serde `rename_all = "kebab-case"` to match
  the JSON config keys (`plan-introduced`, etc.).
- `HookFiring` struct: `{ event: HookEvent, plan: PlanKey,
  sha: CommitSha }`.

### `crates/cli` — new module `hook_config.rs`

- `HookConfig` struct: `BTreeMap<HookEvent, String>`.
- `load_hook_config(repo) -> HookConfig`: load user-level
  defaults first, then overlay repo-level entries per-event.
- `run_hook(repo, config, firing) -> ()`: spawn `sh -c`,
  set env vars, wait, log.

### `crates/cli/src/cli/wfw.rs`

- New `LifecycleSnapshot` alongside `StartupSnapshot`:
  ```rust
  struct LifecycleSnapshot {
      known_plans: BTreeSet<PlanKey>,
      review_state: BTreeMap<PlanKey, (CommitSha, GateState)>,
      finished_shas: BTreeMap<PlanKey, BTreeSet<CommitSha>>,
  }
  ```
  Tracks plan existence, `(latest_reviewable_sha, gate_state)` for
  same-SHA review detection, and finished-plan SHAs independently
  of `StartupSnapshot` so mid-session introductions are covered.
- `detect_lifecycle_transitions(prev, current_views) -> Vec<HookFiring>`:
  compare previous snapshot to current views, emit firings.
- In the watch loop's refold path: call
  `detect_lifecycle_transitions`, fire any hooks, then continue
  to `derive_work` as today.
- On the initial fold (one-shot path): same detection before
  returning items.
- `plan-finalized` hooks use the lifecycle snapshot's own
  `finished_shas` tracking (not `detect_finished`).

### `.clank/.gitignore`

No change needed — `.clank/hooks.json` would be repo-level config
that operators might want to commit. But per the current ignore
rules, everything under `.clank/` except `plans/` and `finished/`
is ignored. If hooks config should be committed, we need to add
`!/hooks.json` to the gitignore.

Decision: **gitignore `hooks.json`** for now (it's per-machine
config, like agent configs). Operators who want shared hooks can
add `!hooks.json` to their repo's `.clank/.gitignore`.

## Tests

### Unit (`crates/core`)

- `HookEvent` serde round-trip.

### Unit (`crates/cli`)

- `load_hook_config`: repo overrides user per-event; missing
  files return empty config.
- `detect_lifecycle_transitions`: new plan → `PlanIntroduced`;
  same-SHA gate state change → `ReviewReceived`;
  no change → empty.

### Integration

- `wfw` with a hook configured: set up a plan intro commit, run
  wfw, assert the hook command ran (e.g. the hook writes a marker
  file, test checks it exists).
- Hook failure (exit non-zero): wfw still prints items and exits
  0.

## Acceptance criteria

- `clank wfw` fires configured hooks on detected lifecycle
  transitions before printing items.
- Hooks run blocking via `sh -c` with `CLANK_EVENT`, `CLANK_PLAN`,
  `CLANK_REPO`, `CLANK_SHA` env vars.
- Repo-level `.clank/hooks.json` overrides user-level
  `~/.clank/hooks.json` per-event.
- Hook failures are logged but don't fail wfw.
- Lifecycle transitions are role-independent — both master's and
  reviewer's wfw fire the same hooks for the same events.

## Out of scope

- Per-plan hook overrides (plan frontmatter or per-plan config).
- `gate-approved` as a separate event (can be added later by
  detecting the specific transition to `MasterToImplement` or
  `MasterToFinalize`).
- Hook execution outside of wfw (e.g. `clank finish` directly
  firing `plan-finalized`). wfw is the single event-detection
  surface.
