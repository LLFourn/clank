# suppress-preadoption-adhoc

`clank html`'s timeline (and any other `LogEvent` consumer)
shows a flood of `AdHoc` events for commits that landed
before this repo adopted clank. The fold at
`crates/core/src/repo_state.rs:590` emits `LogEvent::AdHoc`
for every code-only commit with no plan attribution,
regardless of whether the repo has ever introduced a plan.

The cheap config flip (`review.adhoc_feedback = false`)
silences ad-hoc as REVIEW work but doesn't keep these
events out of the log. They still render in `clank log`,
`clank html`, and anything else that consumes `LogEvent`.

## Fix

Adoption is a durable historical fact, not a snapshot.
Once ANY plan-lifecycle event has been observed in this
repo's history, the repo stays adopted forever — even if
the user later deletes that plan.

Critical: a naive `self.plans.is_empty() &&
self.finished_plans.is_empty()` predicate is WRONG.
`PlanDeleted` (`crates/core/src/repo_state.rs:507-514`)
removes the plan from `self.plans` AND populates nothing in
`finished_plans`. So intro → delete → plain commit would
flip the predicate back to "pre-adoption" and silently
re-suppress a legitimate AdHoc. (`crates/core/src/repo_state.rs:995-1025`
covers this hard-forget shape today.)

Track adoption as a `bool` on `RepoState`:

```rust
pub struct RepoState {
    ...
    /// Once true, stays true. Set when any plan path is
    /// touched (intro, revise, finalize, or delete) — i.e.
    /// the repo has ever participated in clank. Never
    /// cleared, including across `PlanDeleted`.
    pub adopted: bool,
}
```

Set it on every plan-touching commit — the same branches
in `apply_commit` that push `PlanIntro` / `PlanCommit` /
`PlanFinalized` / `PlanDeleted` log events. Never clear.

Gate the AdHoc emission on `self.adopted`: when false,
skip both the `LogEvent::AdHoc` push AND the `ad_hoc`
bucket entry.

Cache compatibility: `RepoState` is serialized into the
state cache. The cache hot path (`crates/cli/src/rebuild.rs`)
serves an exact-HEAD cache hit DIRECTLY without re-folding,
and ancestor hits only fold forward from the cached state.
So just adding `#[serde(default)]` is unsafe — a stale
post-adoption cache would be served with `adopted: false`
and silently suppress legitimate AdHocs until the next plan
touch.

Bump `CACHE_FORMAT_VERSION` in
`crates/cli/src/state_cache.rs:27` (currently `6` → `7`).
That makes the file extension `.v7.bin` and the version
check at line 103 reject every prior cache as a version
mismatch, forcing a clean re-fold from history. `adopted`
gets re-derived correctly. The new field can still carry
`#[serde(default)]` for forward source-level safety, but
the version bump is what makes the change live cache-safe.

## Surfaces touched

- `crates/core/src/repo_state.rs`:
  - `pub adopted: bool` field on `RepoState`, with serde
    default and `wincode` schema annotations to match the
    rest of the cache-serialized fields.
  - `apply_commit`: set `self.adopted = true` inside every
    branch that handles a plan touch (intro / revise /
    finalize / delete).
  - Wrap the AdHoc emission block in an
    `if self.adopted` guard.
- `crates/cli/src/state_cache.rs:27`: bump
  `CACHE_FORMAT_VERSION` from `6` to `7` so prior caches
  fail the version check and trigger a clean re-fold. The
  cache write/read paths already key the filename on the
  version (`.v{N}.bin`) so old artifacts stay on disk but
  are ignored.

## Tests

- `adhoc_suppressed_before_first_plan_intro` — fold three
  plain commits then a `[foo] intro`; assert the log
  events for the first three are NOT `AdHoc`.
- `adhoc_emitted_after_first_plan_intro` — same shape,
  then add a plain commit AFTER the intro; assert that
  commit IS `AdHoc`.
- `adhoc_emitted_again_after_plan_finalized` — intro,
  finish, then a plain commit; assert it IS `AdHoc`
  (history has the finalize event, so we're adopted).
- `adhoc_emitted_after_plan_deleted_then_plain_commit` —
  intro, delete, then a plain commit; assert that LAST
  plain commit IS `AdHoc`. This is the case codex flagged
  — `plans` AND `finished_plans` are both empty here, so
  the snapshot predicate would be wrong; the `adopted`
  field stays true.
- Existing fold tests stay green — adoption only changes
  the truly-pre-adoption case.

## Out of scope

- Backfilling existing repos. The fix is in the fold; once
  shipped, `clank html` and friends re-render cleanly on
  next run.
- Changing the `ad_hoc` BUCKET semantics for the
  `derive_status` path. That bucket is already
  policy-gated by `review.adhoc_feedback`; this fix is
  about what enters the `LogEvent` stream.
