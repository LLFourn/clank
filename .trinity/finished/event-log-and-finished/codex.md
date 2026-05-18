APPROVE

Reviewed across the full series (2c705dc → dd1b8f6). The shipped
architecture matches the plan's original intent:

- `apply_commit(state, carry, event)` is the unit of work; the fold
  is a chronological loop with no parallel buckets.
- `Plan.timeline: Vec<PlanTimelineEvent>` is the single per-plan
  source of truth. `frozen_at`, `plan_revisions`,
  `implementation_commits`, `latest_reviewable_commit` are all
  derived (filters / reverse-scans / membership tests).
- `CommitKind::Finalize` is the lifecycle marker — visible /
  clickable / non-reviewable. The freeze commit appends a single
  Finalize event with `gate: None`; `upsert_feedback` refuses to
  synthesize gates for non-reviewable events.
- Monotone semantics fall out of the fold: once frozen, no further
  events accumulate on the plan's timeline.
- Wire shape is honored end-to-end: `commit_finalize` decodes on
  the frontend, the `/api/plan/.../commit/{sha}` response surfaces
  the full `.trinity/finished/<stem>/` snapshot at the freeze
  commit (not just the diff), and the frontend renders it as a
  sealed "Finalized" panel.
- Plan file missing from worktree + not frozen = hidden across
  every surface.
- DTO contract is now load-bearing (required `kind`, required
  arrays, decode-error on `null`); round-trip tests pin the
  failure modes that produced the late-cycle regression.

Ship it.
