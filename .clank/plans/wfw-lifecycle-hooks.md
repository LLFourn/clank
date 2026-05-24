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

Track per-plan gate state at startup (`PlanView::waiting_on`).
Track the per-plan **gate state** (`PlanView::gate_state` —
`Unreviewed`, `Approved`, `ChangesRequested`) across refold ticks.
Fire `review-received` when the gate state changes. Gate state is
derived purely from reviewer feedback, not from worktree dirtiness
or `waiting_on` — so a dirty plan file doesn't cause a false
positive, and reviewer feedback that lands while the plan file is
dirty doesn't cause a false negative.

Do NOT key this event off `waiting_on`. `waiting_on` mixes
reviewer-driven transitions (feedback posted) with worktree-driven
transitions (`MasterToCommit` from a dirty plan file) and master-
driven transitions (new commit changing `waiting_on` back to
`FirstReview`). Gate state is the clean signal.

### `plan-finalized`

Reuse `detect_finished`'s existing `StartupSnapshot` comparison.
When `detect_finished` produces `WaitItem::Finished` items, also
fire the hook for each.

## Environment variables

Hook commands receive context via env vars:

| Variable | Description | All events |
| --- | --- | --- |
| `CLANK_EVENT` | Event name (`plan-introduced`, etc.) | yes |
| `CLANK_PLAN` | Plan stem (e.g. `my-feature`) | yes |
| `CLANK_REPO` | Absolute repo path | yes |
| `CLANK_SHA` | Relevant commit SHA | yes |

## Hook execution

Inside `wfw::run`, after `derive_from_state` (or `check_once`)
produces a non-empty result:

1. Detect lifecycle transitions by comparing current state to the
   snapshot.
2. For each detected transition, look up the configured hook
   command.
3. Run each hook via `sh -c <command>` with the env vars set.
   Block until the hook exits. Log stderr to wfw's stderr. If the
   hook exits non-zero, log a warning but don't fail wfw — hooks
   are best-effort notifications, not gates.
4. Print items and exit as today.

For the watch loop (long-poll mode): hooks fire on each refold
tick that produces transitions, not just the final one that also
produces work items. This means hooks can fire multiple times
during one wfw invocation (e.g. plan-introduced fires on tick 3,
review-received fires on tick 7 when feedback lands).

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
  `{ known_plans: BTreeSet<PlanKey>, gate_states: BTreeMap<PlanKey, GateState> }`
  where `GateState` is the gate's approval status (not `WaitingOn`).
- `detect_lifecycle_transitions(prev, current_views) -> Vec<HookFiring>`:
  compare previous snapshot to current views, emit firings.
- In the watch loop's refold path: call
  `detect_lifecycle_transitions`, fire any hooks, then continue
  to `derive_work` as today.
- On the initial fold (one-shot path): same detection before
  returning items.
- `plan-finalized` hooks piggyback on the existing
  `detect_finished` output.

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
  gate state change to master-facing variant → `ReviewReceived`;
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
