# finish-squash-idempotent-on-finished

`clank finish --squash <msg>` is the only path that squashes a
plan's commits while PRESERVING its `.clank/` record (unlike
`clank purge --squash`, which always strips `.clank/`). But it
only works on a plan that is still *ready to finish* — run it on
an *already-finished* plan and it no-ops:

```
$ clank finish --squash "Implement foo"
`clank/foo.md` is already finished; nothing to do
```

So if you finish a plan first and only later decide to squash
its history (the common case — you often don't know you want a
squash until after the finish has landed), there is no way to do
a keep-`.clank/` squash. Make `finish --squash` (and `--purge`)
idempotent on already-finished plans: skip the finalize step
(it's already done) and run the rewrite/squash directly.

## Root cause

In `crates/cli/src/cli/finish.rs::run`, the dispatch is:

- A special branch already handles `--amend` on an
  already-finished plan — it skips `finalize()` and optionally
  runs the post-finalize rewrite (finish.rs:35-46).
- For the NON-amend path, `dispatch_readiness` returns `false`
  on `FinalizeReadiness::AlreadyFinished`, printing "already
  finished; nothing to do" and returning BEFORE
  `run_post_finalize_rewrite` is ever reached (finish.rs:48-50
  and 190-192).

So a plain `finish --squash`/`--purge` on a finished plan is
swallowed by the readiness no-op, even though the rewrite engine
is perfectly capable of running over a finished plan's range.

## The fix

Add a branch for `AlreadyFinished && (args.purge ||
args.squash.is_some())` that skips `finalize()` and goes
straight to `run_post_finalize_rewrite` — mirroring the existing
`--amend`+AlreadyFinished branch but WITHOUT the
`amend_already_finished` HEAD re-commit (we are not changing the
finalize commit, only rewriting/squashing the plan's range).

- `--dry` must still route to `dry_run_finish_composite`.
- `include_finalize` continues to derive from `args.purge`
  (finish.rs:144), so `--squash`-only keeps PRESERVING the
  `.clank/finished/<stem>.md` snapshot and `--purge` strips it.
- Plain `clank finish` (no purge/squash) on an already-finished
  plan KEEPS the current "nothing to do" no-op — only the
  rewrite-bearing flags become idempotent.

`run_post_finalize_rewrite` already supports finished plans:
`build_rewrite_preview` re-derives the range from history via
`re_fold_finished_plan_natives` (crates/cli/src/preview.rs), so
no active per-plan timeline is required.

## Surfaces

- `crates/cli/src/cli/finish.rs::run` — add the AlreadyFinished
  + (purge|squash) branch ahead of the `dispatch_readiness`
  gate.
- Consider factoring the shared "skip finalize, run rewrite"
  tail so the `--amend` and non-amend AlreadyFinished branches
  don't duplicate the dry-run + rewrite dispatch.

## Open questions to pin when sized

- **Re-squash of an already-single-commit range**: a second
  `finish --squash` should collapse to the same one commit.
  Confirm the engine handles a 1-commit range cleanly and decide
  whether to detect "already one commit" and skip rather than
  churn the SHA / author date.
- **`--purge` on an already-purged plan**: `.clank/` is already
  gone, so the strip set is empty. Verify the rewrite exits
  cleanly instead of erroring on an empty range.
- **Output wording**: when the op runs but changes nothing,
  decide whether "squashed `<stem>` in place" is honest enough
  or should say "already squashed / no change".

## Out of scope

- A standalone `clank rewrite --squash` or a
  `purge --keep-clank` flag (separate, larger surface). This
  plan only un-blocks the existing `finish --squash` path on
  finished plans.

## Status

Stub — queued 2026-06-09 (lloyd: make `clank finish --squash`
idempotent on already-finished plans so you can do a
keep-`.clank/` squash after the finish has already landed).
