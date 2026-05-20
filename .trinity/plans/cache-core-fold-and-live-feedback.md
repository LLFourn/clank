# cache-core-fold-and-live-feedback

Make Trinity's core repo-state fold cacheable by separating the
commit-derived state from live working-tree feedback, then add a
commit-keyed binary cache for the expensive part of `trinity status`.

## Why

`trinity status` now rebuilds state locally, which is the right source
of truth for daemon-free CLI commands, but it is slow because every
invocation walks the full first-parent history and replays every
commit from scratch.

The obvious cache boundary is HEAD:

- fold git history up to commit `X`
- serialize that commit-derived state under `.trinity/cache/<X>`
- on the next command at the same `X`, load it instead of replaying
  history

The current model is not ready for that because the fold also reads
working-tree feedback files and attaches them to gates while each
commit is processed. That makes the folded state depend on mutable,
uncommitted files under `.trinity/feedback/`, so caching the whole
`RepoState` under a commit hash would be wrong: editing feedback would
require invalidating a cache entry whose key did not change.

The architecture needs a clean split:

- cacheable state: facts derived from git commits only
- live overlay: mutable feedback files for unfinished plans, attached
  after loading or deriving the cacheable state

Committed finalize approvals under `.trinity/finished/<stem>/` remain
part of the commit fold. They are in git history and are safe to cache.

## Current State

- `src/git_io.rs::snapshot` collects both first-parent commit events
  and working-tree feedback files.
- `src/disk_snapshot.rs::DiskSnapshot` contains
  `feedback_files: Vec<FeedbackBlob>`.
- `src/disk_snapshot.rs::derive_state` builds `FoldCarry` from those
  feedback files before replaying commits.
- `FoldCarry` holds `feedback_by_plan`, and `build_gate` pulls
  feedback into each reviewable commit as the commit is folded.
- `RepoState`, `Plan`, `PlanTimelineEvent`, `CommitGate`, and
  `Feedback` live in `trinity-core` and already derive serde
  `Serialize` / `Deserialize`, but they currently represent a
  post-feedback state.

This means there is no stable "git-only state" value to cache.

## Non-Goals

- Do not cache live `.trinity/feedback/` files for active plans.
- Do not invent partial-history checkpointing in this plan. Cache by
  exact HEAD commit first.
- Do not make the daemon the source of truth for CLI status.
- Do not optimize by hiding correctness problems behind broad cache
  invalidation.
- Do not change finalized approval semantics. `.trinity/finished/`
  files committed in git are still folded normally.

## Design

### Git-Only Fold

Split the fold into two explicit stages:

```rust
pub fn derive_base_state(repo_root: PathBuf, snapshot: CommitSnapshot) -> RepoState;

pub fn attach_live_feedback(
    state: &mut RepoState,
    feedback_files: Vec<FeedbackBlob>,
);
```

`derive_base_state` consumes only commit-derived data:

- `head`
- chronological `CommitEvent`s
- committed plan file changes
- committed finalize snapshot changes

It must not read or accept working-tree feedback files.

The base state still includes reviewable timeline events and gates,
but those gates have no live feedback attached yet. They may still
contain data derived from committed finalize snapshots through the
existing finish/frozen-plan path.

`attach_live_feedback` is the only stage that reads mutable
`.trinity/feedback/` data. It attaches feedback to existing
reviewable timeline events for non-finished plans, updates the gate
state, updates participant/missing lists, and updates
`last_activity_ts` from feedback mtimes.

Finished plans are sealed. Live feedback that targets commits in a
finished plan may remain visible as informational metadata if current
surfaces already show it, but it must not change finished-state
semantics. Prefer keeping the first implementation conservative:
only active plans receive live gate updates.

### Snapshot Types

Rename the current overloaded snapshot shape:

```rust
pub struct CommitSnapshot {
    pub head: Option<CommitSha>,
    pub history: Vec<CommitEvent>,
}
```

Move working-tree feedback collection out of `git_io::snapshot`.
Expose it separately:

```rust
pub fn collect_feedback_files(repo_root: &Path) -> Result<Vec<FeedbackBlob>, GitIoError>;
```

Then rebuild becomes:

```rust
let base = load_or_build_base_state(repo_root).await?;
let feedback = git_io::collect_feedback_files(repo_root)?;
let mut state = base;
attach_live_feedback(&mut state, feedback);
```

This makes the cache boundary obvious and testable.

### Binary Cache

Add a small cache module, for example `src/state_cache.rs`.

Cache key:

```text
.trinity/cache/repo-state/<head-sha>.<format-version>.bin
```

The cache payload is the git-only `RepoState` after
`derive_base_state`, before live feedback attachment.

The cache must include enough metadata to reject stale entries:

- cache format version
- Trinity binary/cache schema version
- repo root or repo basename, if needed for sanity checking
- HEAD SHA

Use a binary encoder that is boring and easy to maintain. `bincode`
is the default choice unless a small spike shows `wincode` is a
materially better fit for stable Rust, serde/newtype support, and
maintenance. Do not let encoder selection become a design project.

`trinity-core` model types should be made encodable/decodable by the
chosen encoder. Keep derives centralized on the core model types; do
not introduce hand-written stringly mappers for the cache format.

### Cache Lifecycle

On `trinity status` and other local CLI state reads:

1. Resolve repo root.
2. Read HEAD SHA.
3. Try `.trinity/cache/repo-state/<head>.<version>.bin`.
4. If it loads and validates, use it.
5. Otherwise fold commit history with `derive_base_state`.
6. Write the cache atomically.
7. Collect live feedback files.
8. Attach live feedback.
9. Project status output.

Atomic write should be best-effort but real:

- write to a temp path under `.trinity/cache/repo-state/`
- fsync is optional for now
- rename into place
- on write failure, warn and continue with the freshly folded state

Prune policy should be intentionally simple. Keep a small fixed number
of recent cache files per repo, such as 8 or 16, sorted by mtime. No
LRU database.

### Runtime And Watcher

The daemon can use the same cache path on cold start and HEAD-change
rebuilds, but live feedback updates from the watcher must still apply
in memory immediately.

After the split:

- `Runtime::add_repo` / `head_changed` loads or builds base state,
  then attaches current feedback files.
- `FeedbackWritten` and `FeedbackRemoved` continue to update in-memory
  gates without requiring a full commit fold.
- The runtime should not write live-feedback-bearing `RepoState` into
  the commit cache.

### API Boundary

Most response code should continue to consume ordinary `RepoState`.
The distinction is internal:

- `RepoState` after `derive_base_state`: cacheable base
- `RepoState` after `attach_live_feedback`: full live state for
  current projections

If this distinction is too easy to misuse, add lightweight newtypes:

```rust
pub struct BaseRepoState(RepoState);
pub struct LiveRepoState(RepoState);
```

Use them only if they prevent real mistakes; do not create wrapper
ceremony for its own sake.

## Testing

Add focused tests before broad integration tests:

1. `derive_base_state` does not accept or attach working-tree
   feedback.
2. `attach_live_feedback` attaches feedback to the matching
   non-finished plan commit and updates gate state.
3. Editing a feedback file changes the live state after attachment
   without changing or invalidating the base cache key.
4. Finished plans do not have their lifecycle changed by live
   feedback files.
5. A cached base state plus feedback attachment matches a full
   uncached rebuild for active-plan projections.
6. Corrupt cache files are ignored and replaced.
7. Cache entries for the wrong HEAD or format version are ignored.
8. `trinity status` is measurably faster on a warm cache in a fixture
   with enough commits to exercise the path.

Run:

```sh
cargo test --workspace --exclude trinity-frontend
cargo clippy --all-targets
cargo fmt -- --check
```

## Phases

### Phase 1: Split Snapshot And Fold Inputs

Introduce `CommitSnapshot` and move feedback collection out of
`git_io::snapshot`. Keep behavior unchanged by calling
`derive_base_state` followed immediately by `attach_live_feedback`.

### Phase 2: Live Feedback Attachment

Move feedback indexing, verdict parsing, gate recomputation, and
feedback-driven `last_activity_ts` updates out of `FoldCarry` into
`attach_live_feedback`. Remove `feedback_by_plan` from the commit
fold carry.

### Phase 3: Binary Encoding

Make the core model types encodable with the selected binary encoder.
Add round-trip tests for representative `RepoState` values including
plan-only, code-only, mixed, multi-plan, and finalize timeline events.

### Phase 4: Cache Module

Add `state_cache` with load, validate, atomic write, and prune
helpers. Cache only base states keyed by HEAD and format version.

### Phase 5: Use Cache In Local Rebuilds

Switch `rebuild_repo` or a new `rebuild_repo_cached` entrypoint to
load/build the base state, attach live feedback, and return the live
state. Use it from `trinity status` first, then from other local CLI
commands if the semantics are identical.

### Phase 6: Daemon Adoption

Let daemon cold-start and HEAD-change rebuilds use the same cached
base-state path. Keep watcher feedback updates as incremental
in-memory live overlays.

## Acceptance Criteria

- The commit fold is independent of mutable `.trinity/feedback/`
  files.
- Live feedback is attached in an explicit post-fold stage.
- Base repo state can be binary-encoded and decoded without losing
  typed core invariants.
- `.trinity/cache/` stores base states keyed by HEAD and cache format
  version.
- Editing feedback changes current projections without requiring a
  commit refold and without poisoning the HEAD-keyed cache.
- `trinity status` uses the cache and remains correct when feedback is
  edited between invocations.
- Existing daemon and CLI projections keep their behavior.
