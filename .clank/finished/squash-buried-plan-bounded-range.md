# squash-buried-plan-bounded-range
# `finish --squash` collapses a BURIED plan's own range, restacking what's on top

## Bug (verified live on this repo)

`clank finish <plan> --squash` anchors its rewrite range at HEAD:
`build_rewrite_preview` (preview.rs) fetches `finalized_at` for finished plans
but uses it ONLY for attribution — the range is `metas[intro..HEAD]`. So a
retroactive squash of a buried (already-finished, not-at-HEAD) plan tries to
collapse `[plan-intro .. HEAD]` — sweeping every later plan and ad-hoc commit
into the collapse — and the foreign-commit guard (correctly) refuses.

Repro here: `clank finish setup-defaults-autosquash --squash "…" --dry` →
`range: 82f4277..3bd7882 (4 commit(s) would collapse into one)` where 3bd7882
is HEAD (two other plans' commits swept in). On frostsnap's
`full-app-sim-driver` branch an old plan reports a 290-commit range, 282
foreign.

Why dogfooding never hit it: fresh finish/autosquash squashes at finalize
time, when finalize IS HEAD, so `[intro..HEAD] == [intro..finalize]`. Only
retroactive squash of a buried plan bites. The foreign refusal is the symptom;
HEAD-anchoring is the disease (the guard is also what currently prevents a
mangled collapse — keep it, scope it).

## Desired behavior

`finish <plan> --squash MSG` works for any plan position in history:

- Collapse ONLY the plan's own contiguous `[intro .. finalized_at]` run into
  ONE commit: message = MSG (tag-ensured), tree = **the finalize commit's
  tree** (the plan's cumulative end state — NOT HEAD's tree; today's
  `apply_squash` gets away with HEAD's tree only because it collapses to
  HEAD), parent = intro's parent. Use `squash_commit(source=finalized_at, …)`
  so author preservation + date pinning keep the idempotent-sha property.
- RESTACK every commit after `finalized_at` up to HEAD on top of the squash,
  preserved individually via `replay_commit` — the same
  reword-and-replay shape `reword_in_place` (rewrite.rs) already implements
  for `finish -m`; this is that machinery generalized from "replace one
  commit" to "replace a contiguous run".
- Net effect (plain `--squash`): HEAD's tree unchanged; the plan's N commits
  become 1; later history intact, rewritten shas.
- Migrate feedback for ALL rewritten commits (`rewire::migrate_feedback_pairs`
  — update-ref fires no post-rewrite hook), like reword does.
- Conditional `update_ref(Match(head))` + `reset_hard`, like reword.

## `--purge` subtlety (the critique missed this)

With `--purge` the plan's artifacts (incl. the finalize snapshot) must leave
history — but restacked descendants' trees still CONTAIN the inherited
`.clank/finished/<stem>.md`. Restack steps must therefore carry strip_paths in
purge mode (`strip_tree` before `replay_commit`, i.e. the engine's existing
Rewrite disposition applied to restack steps). Plain `--squash`
(include_finalize=false) restacks trees verbatim.

## Foreign-commit guard: scope to the plan's own range

- Foreign commits INSIDE `[intro..finalized_at]` (mid-plan untagged ad-hoc
  commits): still REFUSE — this is a real boundary, not conservatism: the
  engine replays full trees, so replaying an interleaved foreign commit after
  the squash would reset the tree to its mid-plan state, losing the plan's
  later changes. Name the offending shas/subjects in the error so the operator
  knows what to re-tag or move.
- Commits AFTER `finalized_at`: never foreign-refused — they're restack steps.

## One-computation rule (established by finish-dry-delegates-to-engine)

`--dry` for the bounded squash must be the SAME computation the live run
applies — thread `dry` through the engine; no hand-rolled preview text in
finish.rs. The dry output should show the bounded range
`[intro..finalized_at]`, the squash target tree, and the restack count.

## Scope / non-goals

- Only the retroactive (already-finished) `--squash`/`--purge` range is
  re-bounded. Fresh finish/autosquash is UNAFFECTED (finalize is HEAD; ranges
  coincide).
- `finish -m` reword path untouched (already handles buried plans).
- Active-plan (not-yet-finished) `--squash` semantics unchanged.

## Files
- `crates/cli/src/preview.rs` — bound the finished-plan range at
  `finalized_at`; classify restack steps distinctly (or emit restack info).
- `crates/cli/src/cli/rewrite.rs` — bounded squash + restack (generalize the
  `reword_in_place` shape); scoped foreign guard with named offenders.
- `crates/cli/src/cli/finish.rs` — no preview text; wire dry through.

## Acceptance
- `finish <buried-plan> --squash MSG --dry` reports range == the plan's own
  commit count, restack count == commits after finalize, no foreign refusal
  when none interleaved; live run collapses just those commits; HEAD's tree
  byte-identical (plain squash); later commits preserved individually with
  new shas; feedback migrated.
- Interleaved-foreign fixture: refused with the offending commits NAMED, in
  both dry and live (same computation).
- `--purge` on a buried plan: restacked descendants' trees no longer contain
  the plan's `.clank/` artifacts.
- Fresh autosquash regression suite untouched and green; clippy baseline.

## Implementation notes (as built)

- `RewritePreviewResponse.squash_tip: Option<CommitSha>` — `finalized_at` for
  finished plans, `None` for active; `head_strip_paths` now computed at the
  squash tip's tree when set.
- Engine (`rewrite::run`): boundary = position of `squash_tip` in the steps;
  foreign guard scoped to `steps[..=boundary]` and NAMES offenders (short sha
  + subject); collapse via `apply_squash(source=squash_tip)`; restack of
  `steps[boundary+1..]` reuses the manifest dispositions — so `--purge`
  stripping applies to restacked trees for free (the preview already computes
  their strip_paths).
- `RewriteOutcome.pairs: Vec<(old,new)>` — every replayed/collapsed commit
  (squash maps all collapsed olds to the one squash commit; the migrator keeps
  the latest old, i.e. the finalize's feedback). finish.rs AND purge.rs now
  call `rewire::migrate_feedback_pairs` after live rewrites — previously
  squash/purge rewrites silently stranded sha-keyed feedback (only reword
  migrated); restacked commits carry later plans' gate feedback, so this is
  load-bearing.
- Dry print shows the bounded range + restack count; one-computation rule
  holds (dry threads through the same boundary/blockers the live run uses).
- Verified on this repo: the intro's repro now reports
  `range: 82f4277..82f4277 (1 commit)` + `restacked on top: 3 later
  commit(s)` instead of sweeping to HEAD.
