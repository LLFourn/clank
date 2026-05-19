# mcp-context-surface-cleanup

Tighten the MCP tool catalog so it coordinates work rather than
transports content or carries diagnostic stubs. Four concrete
changes; one architectural goal.

## Why

The MCP surface today mixes three different jobs:

1. **Coordination** (`wait_for_work` — the real value).
2. **Content transport**: `get_context` returns the daemon's
   full per-plan fold — `commits[]` with all gates and feedback
   bodies, full plan markdown, archived cycles, every timeline
   event. The most recent `get_context` call in this session
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
`/mcp__trinity__get_context` returns `PlanDetailResponse` (the
same DTO HTTP `/api/plan/<id>` returns) — full timeline, all
commits with gates and feedback, plan body markdown,
`archived_cycles`, etc. That's a UI shape, not a coordination
shape.

The new `work_context` response is just enough to act on the
latest WFW result:

```rust
pub struct WorkContextResponse {
    pub plan_id: String,
    pub repo: String,
    pub current_path: String,
    pub phase: PlanPhase,
    pub waiting_on: WaitingOn,
    pub latest_relevant_commit: Option<CommitSha>,
    pub review_target: Option<ReviewTarget>,  // sha + kind
    pub feedback_locations: Vec<FeedbackLocation>,
}

pub struct FeedbackLocation {
    pub path: String,           // ".trinity/feedback/foo/abc/codex.md"
    pub role: FeedbackRole,     // Read | Write
}
```

Dropped: `commits[]`, `latest_plan_revision`, `archived_cycles`,
`plan_body`, full timeline. UI consumers keep using the
`/api/plan/<id>` HTTP endpoint, which is unchanged.

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

WFW's plan_id inference (already implemented at
`src/server/wait.rs::compute_match` / the upstream resolver)
consults this BEFORE raising `ambiguous_plan`. If a selection
exists for the resolved `(repo, author_label)` pair, use it.

Storage: in-memory only on `Runtime`. Indexed by `(repo,
AgentLabel)`. Cleared on daemon restart. This is operational
state — the same agent could legitimately want different active
plans across sessions, and persisting would surprise more than
help.

Must NOT:
- Create plan files.
- Change review gates.
- Emit timeline events.
- Reach `is_visible` projections (the selection is
  inference-side, not display-side).

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
  → { repo, basename, watched: true|already_watching }
```

`watch_repo` does steps 1–3. `path` defaults to the caller's
cwd; explicit `path` (basename or absolute) supports the rare
"watch a repo I'm not in" case. Idempotent — already-watching is
a non-error.

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
  `work_context`, drop `start_plan`'s description verbosity (add
  the deprecation note), add `watch_repo`, `set_active_work`,
  `clear_active_work` entries.
- `src/server/mcp.rs` — rename dispatcher branch
  `get_context` → `work_context`, add dispatcher branches for
  the three new tools. Keep `echo_cwd` branch (no catalog
  advertisement but curl-able).
- `crates/trinity-core/src/api.rs` — new `WorkContextResponse`
  shape; the old `PlanDetailResponse` stays for HTTP.
- `src/server/mcp_shim/mod.rs` — autofill keys (line ~244)
  update if the new tools take `author_label`.
- `src/server/wait.rs` (or wherever inference lives) — consult
  `set_active_work` selection before raising `ambiguous_plan`.
- `src/runtime.rs` — `BTreeMap<(RepoBasename, AgentLabel),
  PlanKey>` for ephemeral selections; `set_active_work` /
  `clear_active_work` mutators; `resolve_active_plan` lookup
  used by inference.

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
- `set_active_work` + WFW infers correctly: with a selection,
  multiple-active-plans does NOT raise `ambiguous_plan`.
- `watch_repo` registers a fresh repo; re-watching is idempotent.
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
