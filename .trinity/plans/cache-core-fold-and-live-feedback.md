# cache-core-fold-and-live-feedback

Make Trinity's core repo-state fold cacheable by separating the
commit-derived state from live working-tree feedback, then add a
commit-keyed binary cache for the expensive part of `trinity status`.

## Why

`trinity status` now rebuilds state locally — the right source of
truth for daemon-free CLI commands, but slow: every invocation
walks the full first-parent history and replays every commit from
scratch.

The natural cache boundary is HEAD:

- fold git history up to commit `X`
- serialize that commit-derived state under `.trinity/cache/<X>`
- on the next command at the same `X`, load it instead of replaying

The current model can't be cached as-is because the fold reads
working-tree feedback files and attaches them to gates DURING
commit replay. The folded `RepoState` therefore depends on
mutable, uncommitted files under `.trinity/feedback/`. A
HEAD-keyed cache of that state would be wrong: editing feedback
would have to invalidate a cache entry whose key did not change.

The architecture needs a clean split:

- **base state**: facts derived from git commits only — cacheable
- **live overlay**: mutable feedback files for unfinished plans,
  attached on top of the base state

Committed finalize approvals under `.trinity/finished/<stem>/`
stay in the commit fold. They live in git history and are safe to
cache.

## Current State

- `src/git_io.rs::snapshot` collects both first-parent commit
  events AND working-tree feedback files.
- `src/disk_snapshot.rs::DiskSnapshot` carries
  `feedback_files: Vec<FeedbackBlob>`.
- `src/disk_snapshot.rs::derive_state` builds `FoldCarry` from
  those feedback files before replaying commits.
- `FoldCarry` holds `feedback_by_plan`; `build_gate` pulls
  feedback into each reviewable commit as the commit is folded.
- `RepoState`, `Plan`, `PlanTimelineEvent`, `CommitGate`, and
  `Feedback` already derive `Serialize` / `Deserialize` in
  `trinity-core`, but they currently represent a post-feedback
  state.

There is no stable "git-only state" value to cache today.

## Non-Goals

- No cache of live `.trinity/feedback/` files for active plans.
- No partial-history checkpointing. Cache by exact HEAD commit.
- No making the daemon the source of truth for CLI status.
- No optimization that hides correctness behind broad invalidation.
- No change to finalized approval semantics. `.trinity/finished/`
  files committed in git are still folded normally.

## Design

### Git-Only Fold

Split the fold into two explicit stages:

```rust
pub fn derive_base_state(
    repo_root: PathBuf,
    snapshot: CommitSnapshot,
) -> BaseRepoState;

pub fn attach_live_feedback(
    base: BaseRepoState,
    feedback_files: Vec<FeedbackBlob>,
) -> LiveRepoState;
```

`derive_base_state` consumes ONLY commit-derived data:

- `head`
- chronological `CommitEvent`s
- committed plan file changes
- committed finalize snapshot changes

It must not read or accept working-tree feedback files. A unit
test pins this: the function signature only takes `CommitSnapshot`,
and a fixture with working-tree feedback files present produces
the same `BaseRepoState` whether or not those files exist.

`attach_live_feedback` is the only stage that reads mutable
`.trinity/feedback/` data. It must rebuild gates **chronologically
per active plan**, the same way `build_gate_step` does today —
participants are cumulative across reviewable commits, so a
feedback file on an earlier commit semantically affects every
later reviewable gate through the carried participant set.

Algorithm:

1. Index feedback by `(plan_key, target_sha)`.
2. For each active (non-frozen) plan, clear any
   feedback-derived contents from gates currently in the base
   state. Commit-derived sealed approvals from
   `.trinity/finished/<stem>/` stay.
3. Walk that plan's reviewable timeline oldest-to-newest. For
   every reviewable event, rebuild the full `CommitGate` from
   the carried cumulative participant set plus the feedback
   files targeting that specific commit.
4. Update `last_activity_ts` to the max of commit author
   timestamps and attached feedback file mtimes.

A test pins this: a fixture with feedback on commit A and a
later reviewable commit B in the same plan must produce a gate
on B whose `missing` field includes A's reviewer. Patching
only B (without the chronological walk) would fail.

Finished plans are sealed. Live feedback targeting commits in a
finished plan is ignored at this stage — finished-state
semantics are commit-derived only.

### Cacheable / Live Newtypes

Model the boundary explicitly from the start:

```rust
pub struct BaseRepoState(RepoState);
pub struct LiveRepoState(RepoState);
```

Both newtypes wrap the existing `RepoState`. They exist so the
type system makes the cache contract impossible to subvert:

- only `BaseRepoState` is written to the cache
- only `LiveRepoState` is handed to the projections that drive
  `list_plans`, `finish_preview`, `rewrite_preview`, etc.
- a `&LiveRepoState` deref to `&RepoState` is fine for read-only
  projections; the wrappers exist to police construction, not
  read access

This is the same trade-off the plan made in earlier projects: a
trivial newtype that prevents a real class of mistakes is worth
the ten lines of `impl Deref`.

### Snapshot Types

Rename the current overloaded snapshot:

```rust
pub struct CommitSnapshot {
    pub head: Option<CommitSha>,
    pub history: Vec<CommitEvent>,
}
```

Move working-tree feedback collection out of `git_io::snapshot`:

```rust
pub async fn collect_feedback_files(
    repo_root: &Path,
) -> Result<Vec<FeedbackBlob>, GitIoError>;
```

Then the local rebuild becomes:

```rust
let base = load_or_build_base_state(repo_root).await?;
let feedback = git_io::collect_feedback_files(repo_root).await?;
let state = attach_live_feedback(base, feedback);
```

This is the only sequence callers see; the cache hit/miss is an
implementation detail of `load_or_build_base_state`.

### Binary Cache

Add `src/state_cache.rs`. Cache layout:

```text
.trinity/cache/repo-state/<head-sha>.v<format-version>.bin
```

The cache file's first bytes carry a magic+version header that is
verified before deserialization:

```text
magic: b"TRINITY-BASE-STATE\n"
format_version: u32 (little-endian)
trinity_version: u32 (little-endian)  -- bumped on incompatible
                                         changes to the model
head_sha: 40 bytes ASCII
-- followed by the wincode body
```

The header IS the validation check. Filename version is for
debugging; do not trust the filename to imply the body shape.

**`RepoState.root` is NOT serialized into the body.** An absolute
path baked into a cache payload survives a directory move and
would silently report a stale root in projections that read it
(status, finish_preview, rewrite_preview). On decode, the cache
module injects the *current canonical repo root* into the
deserialized base state; the on-disk payload only carries
commit-derived fields. Either:

- the on-disk body uses a derived "rootless" form of `RepoState`
  (an internal `BaseRepoStatePayload` struct with the same fields
  minus `root`), and `load_or_build_base_state` constructs the
  `BaseRepoState(RepoState { root: <current>, ..decoded })`; or
- the body serializes the full `RepoState` but `root` is
  unconditionally overwritten on decode with the current repo
  root.

Pick whichever is cleaner during implementation — the invariant
is what matters: a moved cache file cannot leak a stale absolute
root into runtime state.

Encoder: **wincode** with its `derive` feature. The cache is an
internal Trinity format — there's no wire compatibility argument
for routing it through serde. Wincode's own `SchemaWrite` /
`SchemaRead` derives own the encoding contract directly; serde
derives stay reserved for HTTP/MCP wire DTOs in `trinity-core`.
No hand-written stringly mappers; no parallel cache schema.

The cache module (`src/state_cache.rs`) calls
`wincode::serialize` / `wincode::deserialize` on the rootless
cache payload type defined in that same module. The payload
itself stays app-local, but wincode's derive model requires the
`SchemaWrite`/`SchemaRead` traits on every transitive field
type — `Plan`, `CommitGate`, `PlanTimelineEvent`, `Feedback`,
`ArchivedCycle`, the id newtypes, `Verdict`, `CommitGateState` —
which all live in `trinity-core`.

So wincode goes into `trinity-core` as an **optional dep behind
a `cache-encoding` feature**, off by default. The app crate
enables it; the wasm frontend and any non-cache consumer build
trinity-core without wincode and pay nothing. Derives are gated
with `#[cfg_attr(feature = "cache-encoding", derive(...))]`.

This is what codex's earlier review anticipated: "wincode lives
wherever the derives need it. If cache payload types live in
trinity-core, then trinity-core can depend on wincode for these
derives." Serde stays the wire contract; wincode is the cache
contract; they don't overlap.

Add `.trinity/cache/` to `.trinity/.gitignore` (the
committed-into-repo gitignore that ships with `trinity init`) so
operators don't accidentally commit cache binaries. Existing
repos that don't have this line get it via a one-line addition;
no migration path needed for a directory that doesn't exist yet.

### CLI Escape Hatch (`--no-cache`)

`trinity status` (and the other local CLI rebuilds in Phase 4)
take a `--no-cache` flag. With the flag set:

- the cache file is not consulted on read
- the cache file is not written on miss
- everything else (live feedback attach, projection) is identical

This is the operator-facing escape hatch for three scenarios:

1. **Real-world A/B timing.** `time trinity status` vs
   `time trinity status --no-cache` in any repo gives an
   honest before/after measurement on real history. Unit-level
   counters prove the cache was hit; this proves the user
   actually feels the difference.
2. **Debug.** If a cache file is suspected of misbehaving and
   the operator hasn't deleted it yet, `--no-cache` produces a
   ground-truth result without forcing a hand-delete.
3. **CI / fixture tests.** Integration tests can force a
   no-cache run as the oracle to compare a cached run against.

`--no-cache` is intentionally a runtime opt-out, not a config
flag. There's no scenario where a user wants caching disabled
permanently — that would be a bug in the cache, not a
preference.

### Cache Lifecycle

On any local rebuild path:

1. Resolve repo root, read HEAD.
2. Try `.trinity/cache/repo-state/<head>.v<version>.bin`.
3. If header validates and body deserializes, use the cached
   `BaseRepoState`.
4. Otherwise fold history with `derive_base_state` and write the
   cache:
   - serialize to bytes
   - write to a sibling tempfile in the same directory
   - rename into place (atomic on same-filesystem POSIX rename)
   - on write failure, log a warning and continue with the
     freshly-folded state — never block the operator on a cache
     write
5. Collect live feedback files.
6. Attach live feedback.
7. Return `LiveRepoState`.

**Concurrent invocations.** Two `trinity status` calls at the
same HEAD must not corrupt each other. Because writes go through
`<unique-tempfile> → atomic rename`, the worst case is one
invocation overwriting the other's cache file — both bodies are
byte-identical at the same HEAD, so the rename race is safe. Do
not add file locks.

**Prune — logarithmic thinning.** After a successful write,
walk the cache directory and apply this policy by file mtime:

1. **Fresh window**: keep every entry with `age < FRESH_HOURS`
   (default 24h). No thinning here — operators working at HEAD
   often have a handful of recent HEADs they ping back to (branch
   tips, just-amended commits).

2. **Thinned region** (`age >= FRESH_HOURS`): bucket each entry
   by `floor(log2(age_hours))`. Keep only the newest entry in
   each bucket; delete the rest.

3. **Floor**: delete any entry with `bucket > MAX_BUCKET`
   (default 16 → ~7.5 years).

The doubling-gap property the user asked for emerges naturally:
bucket N covers ages 2^N to 2^(N+1) hours, so the gap between
retained caches roughly doubles as you go back. Steady-state
total ≈ `FRESH_WINDOW_entries + MAX_BUCKET` (≈ a few dozen).

No LRU database, no manifest file — `mtime` from the filesystem
is the only source of truth. The whole prune is one directory
listing plus a `HashSet<u32>` of "buckets seen". Concrete
constants live in `state_cache.rs` so they're easy to tune.

### Runtime And Watcher

The daemon uses the same cache path on cold start and on
HEAD-change rebuilds. Live feedback updates from the watcher
still apply in memory immediately.

After the split:

- `Runtime::add_repo` / `head_changed` calls
  `load_or_build_base_state`, attaches current feedback files in
  memory, and stores the resulting `LiveRepoState`.
- `FeedbackWritten` / `FeedbackRemoved` events continue to update
  in-memory gates without re-folding commits. They mutate the
  live overlay, never the base.
- The runtime never writes a `LiveRepoState` (or any state with
  feedback attached) into the commit cache.

**Single source of truth for chronological gate rebuilds.** The
per-plan oldest-to-newest walk that `attach_live_feedback` does
is the same walk a single `FeedbackWritten` event needs to
perform when it lands on commit A and affects gates on later
commits B, C, ... — cumulative participants propagate the same
way. Both paths must call the same `rebuild_plan_gates(plan,
feedback_index)` helper. Duplicating this logic is exactly how
the cache path and the live daemon path will drift; the
plan-required test #3 only catches the bulk-attach path, not
the watcher path.

### API Boundary

Most response code keeps consuming `&RepoState` via `Deref<Target=RepoState>`
on `LiveRepoState`. The newtypes police construction at the
fold/cache boundary; reads are unchanged.

The two entry points the rest of the codebase calls:

- `rebuild_repo(repo)` — folds (or loads), attaches, returns
  `LiveRepoState`. The async signature stays the same; only the
  return type narrows from `RepoState` to `LiveRepoState`.
- `state_cache::load_or_build_base_state(repo)` — internal helper
  used by `rebuild_repo`. Public only inside the crate.

## Testing

Focused, not broad. Each test pins one invariant in the new
model:

1. `derive_base_state` is feedback-blind: same input
   `CommitSnapshot` produces byte-identical (via wincode
   round-trip) `BaseRepoState` regardless of what
   `.trinity/feedback/` holds.
2. `attach_live_feedback` attaches a feedback file to the
   matching non-finished plan/commit and updates the gate state.
3. **Cumulative participants survive the split.** Fixture: plan
   with reviewable commits A and B, feedback on A from reviewer
   `alice`. After `attach_live_feedback`, gate B's `missing`
   field lists `alice`. This is the core invariant the
   chronological per-plan walk protects.
4. Editing a feedback file changes the resulting `LiveRepoState`
   without changing the bytes of the cached `BaseRepoState`.
5. Live feedback targeting a commit in a finished plan does NOT
   alter the finished-plan lifecycle or gate.
6. `attach_live_feedback(load_from_cache(), feedback)` matches
   `attach_live_feedback(derive_base_state(snap), feedback)` for
   active-plan projections — full wincode round-trip equivalence.
7. **Moved cache injects current root.** Build a cache at one
   path, copy the `.trinity/cache/` dir to a renamed/relocated
   repo, load. The resulting `BaseRepoState.root` reports the
   new canonical root, not the original. Pins the no-stale-root
   invariant from the design.
8. Corrupt cache files (mangled header, garbage body) are
   ignored and replaced on next write; the rebuild succeeds.
9. Cache entries with mismatched HEAD or format version are
   rejected by the header check.
10. **Cache-hit path skips the fold.** Wire a counter (or
    feature-gated assertion) into `derive_base_state`. Test
    asserts first `rebuild_repo` increments it, second
    `rebuild_repo` at the same HEAD does not. This replaces a
    wall-clock perf assertion — wall-clock thresholds in CI are
    flaky and don't actually prove the cache was hit.
11. **Logarithmic thinning retains coverage at all timescales.**
    Fixture: write N synthetic cache files with `mtime`s
    spanning hours-to-years (touch the files to backdate).
    Run prune. Assert: every entry in the fresh window
    survives; in the thinned region, each `log2(age_hours)`
    bucket contains exactly one survivor; entries beyond
    `MAX_BUCKET` are gone.

Run:

```sh
cargo test --workspace --exclude trinity-frontend
cargo clippy --all-targets
cargo fmt -- --check
```

## Phases

### Phase 1: Split Fold From Live Attach

Introduce `CommitSnapshot`, the `BaseRepoState` / `LiveRepoState`
newtypes, `derive_base_state`, and `attach_live_feedback`. Move
feedback indexing, verdict parsing, gate recomputation, and
feedback-driven `last_activity_ts` updates out of `FoldCarry`
into `attach_live_feedback`. Move feedback collection out of
`git_io::snapshot` into `collect_feedback_files`. Wire
`rebuild_repo` to call both stages back-to-back so external
behavior is unchanged.

Phase 1 ships the architectural model in one piece — the old
plan's Phase 1+2 were inseparable (Phase 1 couldn't claim
"behavior unchanged" without Phase 2's `attach_live_feedback`).

### Phase 2: Binary Round-Trip For Core Types

Add `SchemaWrite` / `SchemaRead` derives to the rootless cache
payload type and any of its constituent types that aren't yet
covered. Verify wincode round-trips `BaseRepoState` across the
model's variants: plan-only, code-only, mixed, multi-plan,
finalize. Add the round-trip tests; if a type needs explicit
schema adjustment, do it here. No cache write/read yet — just
the encode/decode primitives.

### Phase 3: Cache Module

Add `src/state_cache.rs` with magic-header-stamped load,
validate, atomic write, and prune helpers. Cache only
`BaseRepoState` keyed by HEAD + format version. Unit tests for
the corrupt-file and wrong-HEAD paths.

### Phase 4: Use Cache In Local CLI Rebuilds

Wire `state_cache` into `rebuild_repo`. Add the `--no-cache`
flag to `trinity status`, `trinity finish`, `trinity purge` (all
the local CLI rebuild paths). `trinity status` is the first
beneficiary; the others flip on automatically since they share
the same entry point. Confirm the cache-hit counter test
passes.

Real-repo smoke verification at the end of this phase (not a
committed CI test — a manual sanity step the implementor runs
before declaring Phase 4 done):

```sh
# Cold: blow away any existing cache
rm -rf .trinity/cache
time trinity status --no-cache
time trinity status                 # cold path (writes cache)
time trinity status                 # warm path (hits cache)
time trinity status --no-cache      # bypass again, should match cold
```

The warm `trinity status` should be visibly faster than
`--no-cache` on the trinity repo itself (which now has ~150
commits with `.trinity/` history). If it isn't, something is
wrong with the implementation, not the test.

### Phase 5: Daemon Adoption

Daemon cold-start and HEAD-change rebuilds use the same cached
base path. Watcher events for feedback continue as incremental
in-memory live overlays — they must NOT trigger a cache write
or a commit refold.

## Acceptance Criteria

- The commit fold (`derive_base_state`) is independent of
  mutable `.trinity/feedback/` files. Pinning unit test exists.
- Live feedback is attached in an explicit post-fold stage
  (`attach_live_feedback`).
- `BaseRepoState` round-trips through wincode without losing
  typed core invariants (newtype identity, enum tags).
- `.trinity/cache/repo-state/` stores base states keyed by HEAD
  and cache format version, with a magic+version header.
- `.trinity/.gitignore` excludes `.trinity/cache/`.
- Editing feedback changes current projections without requiring
  a commit refold and without poisoning the HEAD-keyed cache.
- `trinity status` uses the cache and remains correct when
  feedback is edited between invocations.
- Cache-hit path provably skips `derive_base_state` (counter
  test, not wall-clock).
- Existing daemon and CLI projections keep their behavior;
  `LiveRepoState` derefs to `&RepoState` for read-only callers.
- Cumulative-participant semantics survive the split: a
  reviewable commit gate lists earlier reviewers in
  `missing` even when no feedback file targets that
  later commit.
- Cached payloads do not carry an absolute repo root that
  survives a directory move; `BaseRepoState.root` is always the
  current canonical root post-load.
- `trinity-core` depends on wincode only through the optional
  `cache-encoding` feature (off by default). Wasm frontend
  builds don't compile wincode at all.
- Cache retention follows a logarithmic-thinning policy: a
  fresh window keeps every recent entry, and the gap between
  retained older entries roughly doubles with age.
- `trinity status --no-cache` (and the equivalent on `finish`
  and `purge`) bypasses both cache read and write, producing
  the same result as a cache-free run.
