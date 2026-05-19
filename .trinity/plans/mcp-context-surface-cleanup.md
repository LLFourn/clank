# mcp-context-surface-cleanup

Tighten the MCP tool catalog so it coordinates work rather than
transports content or carries diagnostic stubs. Four concrete
changes; one architectural goal.

## Why

The MCP surface today mixes three different jobs:

1. **Coordination** (`wait_for_work` — the real value).
2. **Content transport**: `GetContextResponse` (the MCP shape;
   distinct from the UI's `PlanDetailResponse`, which also
   carries the plan body markdown) returns the per-plan fold's
   bulky slices — `commits[]` with every gate and feedback body,
   the full `timeline[]`, `plan_revisions[]`,
   `implementation_commits[]`, `archived_cycles[]`, and a
   `pr_hint`. The most recent `get_context` call in this session
   returned an 80 KB response that the harness had to spool to
   disk because it didn't fit in a tool-result.
3. **Mixed-concern bootstrap**: `start_plan` does TWO things —
   register the cwd-repo with the daemon AND create a plan file
   under `.trinity/plans/`. These have different lifecycles
   (once-per-repo vs. per-plan).

Plus `echo_cwd`, a diagnostic stub that's been in the catalog
since development. It's not a workflow primitive.

The architectural principle: **MCP coordinates work; it does not
transport content.** Anything that wants the full plan view
(timelines, all feedback bodies, full diffs, archived cycles) is
a UI consumer and reads HTTP `/api/*`. The MCP surface stays
small, fast, and focused on "what should I do next."

## What

Four changes, each landable as its own phase. Phases ordered by
risk (low → high) so we can land the easy wins without entangling
them with the harder one.

### Phase 1: Remove `echo_cwd` from the public catalog

- Drop the `echo_cwd` entry from `tools::catalog()`.
- Keep the dispatcher branch in `src/server/mcp.rs:39` so a
  hand-rolled `tools/call` request still works for debugging
  (curl-able), but `tools/list` no longer advertises it.
- Update `src/tools.rs:15` doc comment (currently "Four
  coordination tools (plus the `echo_cwd` diagnostic stub)").

Trivial; sets the tone for the rest.

### Phase 2: Narrow + rename `get_context` → `work_context`

The wire-shape change is the substantive part. Today's
`/mcp__trinity__get_context` returns `GetContextResponse` — a
near-superset of the per-plan fold's bulky slices:
`commits[]` with every gate and feedback body, the full
`timeline[]`, `plan_revisions[]`, `implementation_commits[]`,
`archived_cycles[]`, plus `pr_hint`, `review_gate`, etc.
`PlanDetailResponse` (the UI's `/api/plan/<id>` shape) adds
`plan_body` markdown on top, but MCP doesn't carry that today.
Both are too rich for a coordination call.

The new `work_context` response is just enough to act on the
latest WFW result. It preserves the typed coordination fields
that today's `GetContextResponse` already carries — agents (and
particularly reviewers) need them as first-class outputs, not
loose pieces to reassemble:

```rust
pub struct WorkContextResponse {
    pub plan_id: String,
    pub repo: String,
    pub current_path: String,
    pub lifecycle: PlanLifecycle,
    pub phase: Posture,
    pub plan_worktree_status: PlanWorktreeStatus,
    pub waiting_on: WaitingOn,
    pub expected_action: ExpectedAction,
    pub review_target: Option<ReviewTarget>,
    pub write_feedback: Option<WriteFeedback>,
    pub latest_relevant_commit: Option<String>,
}
```

Preserved (already on `GetContextResponse`): `expected_action`
(the typed work-action enum that maps to WFW), `write_feedback`
(the canonical reviewer write path with target SHA + author),
`review_target`, `waiting_on`, `phase`, `lifecycle`,
`plan_worktree_status`, `current_path`, `latest_relevant_commit`.
These are the coordination outputs.

Dropped: `commits[]`, `timeline[]`, `plan_revisions[]`,
`implementation_commits[]`, `archived_cycles[]`, `review_gate`,
`latest_plan_revision`, `latest_implementation_revision`,
`pr_hint`. These are UI / browse-all-content fields. The
`/api/plan/<id>` HTTP endpoint (which projects from
`PlanDetailResponse`, including `plan_body`) is unchanged for UI
consumers.

Rename `get_context` → `work_context`. No backward-compat alias;
the only consumers are agents we control.

Inputs stay: optional `plan_id` (with the existing cwd-repo
inference), optional `repo`, optional `author_label`. Errors
stay: `invalid_plan_id`, `unknown_repo`, `unknown_plan`,
`plan_not_committed`, `plan_conflict`, `no_active_plan`,
`ambiguous_plan`.

`get_context` (old name) is gone from `tools::catalog()` AND from
the dispatcher. No alias.

### Phase 3: Add `set_active_work` / `clear_active_work`

When multiple plans are active in one repo, WFW raises
`ambiguous_plan` with candidates. Today the only way out is to
pass `plan_id` on every call — awkward for interactive flows.

New ephemeral selection on the runtime:

```text
set_active_work { repo?, plan_id, author_label? } → { ok: true }
clear_active_work { repo?, author_label? } → { ok: true }
```

Selection lookup is consulted at the **plan-id resolver**, not
at `compute_match`. The resolver today is
`src/server/mcp.rs::resolve_plan_id` (called from `get_context`
at line 294 and from the WFW handler before
`run_wait_for_work`). The new logic sits inside `resolve_plan_id`
in this exact order:

1. If `plan_id` is explicit → use it (unchanged).
2. Resolve `repo` (explicit `repo` arg, else cwd).
3. If `author_label` is present (after shim autofill) AND a
   `set_active_work` selection exists for `(repo,
   author_label)`: validate it (rule below). If valid, return
   the selected plan. If invalid, drop the entry from memory
   and fall through.
4. Otherwise count *active + visible* plans in `repo`:
   - 1 candidate → return it.
   - 0 candidates → `NoActives`.
   - 2+ candidates → `Ambiguous` with the candidate list.

`author_label` semantics: schema-optional (shim autofills from
its cache), daemon-side **not** required. If the resolver reaches
step 3 with no `author_label`, **skip** the selection consult
silently and proceed to step 4. Selection is an ergonomic
convenience; missing author is not an error here. Errors only
arise if a downstream operation (e.g. WFW's reviewer write path)
actually needs the label and doesn't have one.

Active-visible consistency (codex point 4): today's
`resolve_plan_id` counts `!p.is_frozen()` plans without checking
`PlanWorktreeStatus`. That's a latent bug — a non-frozen plan
whose file is missing from the worktree is `!is_visible` and
should not count as an active candidate. Phase 3 aligns both
paths on the same active-visible rule: a candidate is
`!is_frozen && plan_worktree_status != PlanFileMissing`. Both
selection validation and normal counting use this rule. If the
behavior change to normal counting breaks any test, that test
was depending on the bug; update it.

Stale-selection rule (the validation in step 3):

- The selected plan exists in the resolved repo's `plans` map.
- The plan is not frozen.
- The plan passes the active-visible check above
  (`plan_worktree_status != PlanFileMissing`).

Any failure → drop the selection from memory and fall through
to step 4.

Lock-boundary discipline (codex point 3): validating the
selection requires `PlanWorktreeStatus`, which reads disk. The
implementation must NOT hold the runtime mutex across disk I/O.
The pattern (mirroring `src/server/wait.rs::compute_match`):

1. Under `runtime.state().lock()`: copy the selected plan's
   `{plan_key, plan_path, body_hash, is_frozen}` into a small
   value and drop the lock.
2. Drop the lock. Call `compute_plan_worktree_status_parts`
   on the copied path/hash.
3. With the disk result in hand, decide use-or-clear. If
   clearing, re-acquire the lock briefly to mutate the
   selection map.

`set_active_work` validation at set time:

- If `repo` arg is supplied alongside `plan_id`, `repo` must
  resolve to the same `RepoBasename` that `plan_id` names.
  Mismatch → invalid-args error.
- The plan must exist in that repo's `plans` map, be
  non-frozen, and pass the active-visible check. Failure →
  typed error
  (`UnknownPlan` / `PlanNotCommitted` / `PlanNotActive` /
  `PlanHidden` — pick the matching existing variant). No
  silent acceptance of a doomed selection.
- The same active-visible check at set time uses the same
  lock-then-disk-read pattern.

Storage: in-memory only on `Runtime`. Indexed by `(RepoBasename,
AgentLabel)`. Cleared on daemon restart. This is operational
state — the same agent could legitimately want different active
plans across sessions, and persisting would surprise more than
help.

Must NOT:
- Create plan files.
- Change review gates.
- Emit timeline events.
- Bypass `is_visible` (validated at consult time, not stored).

If the cleared/missing selection still results in
`ambiguous_plan`, the error message lists candidate `plan_id`s
and points at `set_active_work` in the hint.

### Phase 4: Add `watch_repo`; deprecate (but keep) `start_plan`

Today `start_plan { slug, label }` does:

1. Resolves and canonicalizes the cwd-repo.
2. Registers it with the daemon (`runtime.add_repo`).
3. Updates `.gitignore` to exclude `.trinity/feedback/` and
   `.trinity/cache/`.
4. Creates `.trinity/plans/<slug>.md` (if not exists).
5. Returns `next_step` instructing the caller to `git commit`.

Steps 1–3 are a once-per-repo bootstrap. Step 4 is per-plan.
Step 5 is a documentation hint that doesn't need a tool call.

Split:

```text
watch_repo { path? }
  → { repo, basename, status: WatchRepoStatus }

WatchRepoStatus = "registered" | "already_watching"
```

`status` is a typed enum on the wire (serde `rename_all =
"snake_case"`), not a stringly-typed bool/string union. No
loose ad-hoc values.

`watch_repo` does steps 1–3. `path` semantics:

- absent → caller's cwd (the same flow `start_plan` uses today).
- absolute path → canonicalize and register that root.
- relative path → resolved against cwd, then canonicalized.
- basename-only is NOT accepted for registration (an unknown
  basename has no root to resolve to). Existing tools' `repo`
  field already uses basename for lookup of already-watched
  repos; that semantics stays separate.

Idempotent — already-watching is a non-error
(`watched: already_watching` in the response).

Plan creation becomes a filesystem convention: write
`.trinity/plans/<slug>.md`, commit it. The fold pipeline picks
it up automatically (this already works; it's how `start_plan`'s
post-call commit ends up registering the plan).

`start_plan` stays in the catalog with a deprecation note in its
description: "prefer `watch_repo` + a `.trinity/plans/<slug>.md`
filesystem write. `start_plan` remains as a convenience wrapper
for now." Internally, `start_plan` becomes `watch_repo` +
file-create.

Phase 4 is last because it touches the agent-facing bootstrap
flow. The other three phases land cleanly without it.

## Rules

- One coordination surface, one ownership rule per tool. Don't
  re-introduce content-transport on MCP; UI reads stay on HTTP.
- `work_context` is the only narrowed-projection tool. Don't
  add parallel "give me X" tools that re-duplicate slices of the
  per-plan fold.
- `set_active_work` selection is ephemeral. Don't persist it
  unless we hit a concrete pain point.
- `watch_repo` is idempotent. Re-watching an already-watched
  repo returns `already_watching: true`, not an error.
- No backward-compat aliases for renamed tools. There are no
  third-party MCP clients; the agents we control update with the
  rename.

## Files touched (sketch)

- `src/tools.rs` — drop `echo_cwd` entry, rename `get_context` →
  `work_context` (and tighten its description to match the
  narrowed shape), drop `start_plan`'s description verbosity (add
  the deprecation note pointing at `watch_repo` +
  `.trinity/plans/<slug>.md`), add `watch_repo`,
  `set_active_work`, `clear_active_work` entries.
- `src/server/mcp.rs` — rename dispatcher branch
  `get_context` → `work_context`, add dispatcher branches for
  the three new tools. Keep `echo_cwd` branch (no catalog
  advertisement but curl-able). Inside `resolve_plan_id`, consult
  the `set_active_work` selection at the right point with the
  stale-validation rule above.
- `src/mcp_shim/mod.rs` — autofill key map at line ~243 changes
  `"get_context"` → `"work_context"`, adds entries for the three
  new tools that take `author_label`. The instruction text at
  line ~385 currently tells agents "follow up with
  `get_context({plan_id})`" — rename to `work_context`. Any
  other hard-coded references in the shim's tool-description
  copy update accordingly.
- `crates/trinity-core/src/api.rs` — new `WorkContextResponse`
  shape (the narrowed fields enumerated in Phase 2); the old
  `GetContextResponse` is gone. `PlanDetailResponse` (HTTP)
  unchanged.
- `src/responses.rs` — `get_context_response*` builders become
  `work_context_response*` and project only the new fields.
- `src/runtime.rs` — `BTreeMap<(RepoBasename, AgentLabel),
  PlanKey>` for ephemeral selections; `set_active_work` /
  `clear_active_work` mutators; `lookup_active_plan` accessor
  used by the resolver.

## Testing

- Wire snapshot suite (`crates/trinity-core/tests/wire_snapshots.rs`)
  pins the new `work_context` shape. Old `get_context` snapshots
  are removed; UI HTTP snapshots are unchanged.
- Guard A on `src/server/mcp.rs` updates if any field needs the
  allowlist (likely none — the new shape is fully typed).
- Unit test on `Runtime`: `set_active_work` then
  `resolve_active_plan` returns the selection; `clear_active_work`
  removes it; restart clears.
- Integration test: WFW with multiple active plans + a
  `set_active_work` selection returns work for the selected plan
  (not `ambiguous_plan`).
- Integration test: `watch_repo` on a fresh repo registers it;
  twice is non-error.
- `cargo test --workspace --exclude trinity-frontend`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`

## Acceptance criteria

- `tools::catalog()` no longer advertises `echo_cwd`. Hand-rolled
  `tools/call` for `echo_cwd` still works.
- `tools::catalog()` advertises `work_context` (new),
  `set_active_work`, `clear_active_work`, `watch_repo`. Does not
  advertise `get_context`.
- `work_context` response carries only the narrowed fields
  enumerated above. A serde-deny-unknown round-trip test pins
  the shape.
- `set_active_work` + WFW infers correctly: with a valid
  selection, multiple-active-plans does NOT raise
  `ambiguous_plan`. With a stale selection (frozen / missing /
  wrong repo), the resolver drops it and falls through to the
  normal counting path.
- `mcp_shim` autofill + instruction text reference
  `work_context` (not `get_context`) after the rename.
- `watch_repo` registers a fresh repo; re-watching returns
  `status: "already_watching"` (typed enum, not a stringly
  value).
- `set_active_work` rejects a `repo` arg that doesn't match the
  `plan_id`'s basename, rejects a plan that is frozen / hidden /
  unknown, and otherwise records the selection. Stale entries
  are dropped at use time.
- Normal `resolve_plan_id` counting uses the same
  active-visible rule as the selection validator
  (`!is_frozen && !PlanFileMissing`). Hidden plans no longer
  count as candidates in either path.
- `start_plan` still works (deprecated label, same behavior).
- HTTP `/api/plan/<id>` shape is unchanged; UI tests pass.
- Existing `wait_for_work` behavior is unchanged.

## Non-goals

- Removing `start_plan` entirely. Phase 4 deprecates without
  deleting; a follow-up plan can remove it once agents are
  migrated.
- Inlining feedback bodies in WFW or anywhere else. That's
  the `wfw-inline-feedback-body` stub.
- New work-action variants. That's the
  `wfw-multi-phase-completion-flag` stub.
- CLI changes. That's the `trinity-wfw-cli` stub.
- Tool-description nudges for quote-first / no-filesystem-search.
  That's the `agent-feedback-paraphrasing-guard` stub.

## Trade-off honest record

The big call here is "**no backward-compat alias** for the
`get_context` → `work_context` rename." This breaks any agent
calling `get_context` until they update. Justification: the only
consumers are agents we control (Claude Code, Codex), and the
new shape is intentionally smaller — keeping the old name pinned
to the old shape forever defeats the cleanup. The migration
window is "the time it takes to redeploy agents," not "forever."

The smaller call: keeping `echo_cwd`'s dispatcher branch live
even though it's gone from the catalog. Costs nothing
(~3 lines); preserves the diagnostic when debugging stdio
shenanigans.

The judgment call on `set_active_work` persistence: we punted to
in-memory. If users hit "I restarted the daemon and lost my
selection" repeatedly, we add a cache-file persister in a
follow-up. Don't over-build on speculation.
