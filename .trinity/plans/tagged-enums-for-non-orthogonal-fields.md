# tagged-enums-for-non-orthogonal-fields

Replace the three remaining "flat struct with `kind` discriminator
+ sibling fields whose validity depends on `kind`" antipatterns
with `#[serde(tag = "kind")]` enums. The
`mcp-context-surface-cleanup` plan just did this for
`ExpectedAction`; this plan finishes the job everywhere else the
pattern appears.

## Why

Each of the three structs encodes the same architectural smell:
the type system says "all four fields are populated" while the
documentation says "actually field X is only present when kind ==
Y." That's an invariant in prose, not in types. Tagged enums make
it an invariant in types — invalid combinations can't be
constructed AND the wire shape shrinks (no `"old_lineno":null`
noise).

A thorough survey of `crates/trinity-core/src/{api,model,vocab}.rs`
plus daemon internals found exactly three clear instances.
Anything else with a `kind`-like field either already uses
`#[serde(tag = ...)]` or has uniform sibling fields and is fine.

### The three structs

**1. `CommitRow`** at `crates/trinity-core/src/api.rs:59` and
**2. `PlanTimelineEvent`** at `crates/trinity-core/src/model.rs:71`:

Both share the same shape and the same antipattern:

```rust
pub struct X {
    pub sha: ...,                     // (or sha+author_ts+subject on PlanTimelineEvent)
    pub kind: CommitKind,             // discriminator
    pub gate: Option<CommitGate>,     // Some iff kind.is_reviewable()
    pub feedback: Vec<Feedback>,      // (CommitRow only) empty iff !reviewable
}
```

Both struct docs literally spell out the invariant:
> `gate` is `Some` for reviewable kinds (PlanOnly, CodeOnly,
> Mixed); `None` for non-reviewable kinds (MultiPlan, Finalize).

**3. `DiffLine`** at `crates/trinity-core/src/api.rs:233`:

```rust
pub struct DiffLine {
    pub kind: DiffLineKind,           // Insert | Delete | Context | Meta
    pub content: String,
    pub old_lineno: Option<usize>,    // Some for Delete, Context
    pub new_lineno: Option<usize>,    // Some for Insert, Context
}
```

At-scale wire benefit: thousands of DiffLines per diff. Today
every Insert line carries `"old_lineno":null`, every Delete line
carries `"new_lineno":null`, every Meta line carries both. Tagged
enum drops the null entries.

## What

Three tagged enums replacing three flat structs. Naming chosen to
match each variant's role on the wire (snake_case).

### 1. `CommitRow` → tagged enum

```rust
#[derive(...)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitRow {
    PlanOnly  { sha: String, gate: CommitGate, feedback: Vec<Feedback> },
    CodeOnly  { sha: String, gate: CommitGate, feedback: Vec<Feedback> },
    Mixed     { sha: String, gate: CommitGate, feedback: Vec<Feedback> },
    MultiPlan { sha: String },
    Finalize  { sha: String },
}
```

Yes, the three reviewable variants share the same payload shape.
That's the honest model: those three CommitKinds carry a gate +
feedback; the other two don't. Keeping five variants preserves
the kind distinction on the wire (still `"kind":"plan_only"` vs
`"kind":"code_only"`) without splitting `CommitKind` into a
sub-enum.

Note that `CommitGate` becomes non-Option inside the reviewable
variants — the "is_reviewable implies Some" invariant is now
structural.

### 2. `PlanTimelineEvent` → tagged enum

Same shape as `CommitRow` but the daemon-fold side also carries
`author_ts: i64` and `subject: String`:

```rust
#[derive(...)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanTimelineEvent {
    PlanOnly  { sha: CommitSha, author_ts: i64, subject: String, gate: CommitGate },
    CodeOnly  { sha: CommitSha, author_ts: i64, subject: String, gate: CommitGate },
    Mixed     { sha: CommitSha, author_ts: i64, subject: String, gate: CommitGate },
    MultiPlan { sha: CommitSha, author_ts: i64, subject: String },
    Finalize  { sha: CommitSha, author_ts: i64, subject: String },
}
```

`PlanTimelineEvent` is daemon fold-state (lives in
`trinity-core::model`), not just wire. Replacing it touches
`disk_snapshot::apply_commit` (the only writer), all the
projection functions that walk `plan.timeline` (event_for,
event_for_mut, frozen_at, latest_reviewable_commit_for, etc.),
plus every test that constructs a fixture event.

### 3. `DiffLine` → tagged enum

```rust
#[derive(...)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffLine {
    Insert  { content: String, new_lineno: usize },
    Delete  { content: String, old_lineno: usize },
    Context { content: String, old_lineno: usize, new_lineno: usize },
    Meta    { content: String },
}
```

The lineno fields are now non-Option inside the variants where
they're meaningful. Meta carries only `content` (the `@@ ... @@`
hunk-header / file-header strings).

### Cleanup

`vocab::DiffLineKind` is fully consumable into the tagged
`DiffLine` — delete it.

`CommitKind` stays for `Plan::frozen_at`'s scan
(`matches!(e.kind, CommitKind::Finalize)`) and any other
projection that matches across all five kinds without caring
about the per-kind payload. The tagged enums replace its USE as
a struct field on `CommitRow` / `PlanTimelineEvent`, not the
closed-vocab enum itself.

## Files touched (sketch)

- `crates/trinity-core/src/api.rs` — replace `CommitRow` struct
  with tagged enum; replace `DiffLine` struct with tagged enum.
- `crates/trinity-core/src/model.rs` — replace `PlanTimelineEvent`
  struct with tagged enum.
- `crates/trinity-core/src/vocab.rs` — delete `DiffLineKind`.
  Keep `CommitKind` (used elsewhere as a value-type).
- `crates/trinity-core/src/lib.rs` — drop `DiffLineKind` re-export.
- `crates/trinity-core/tests/wire_snapshots.rs` —
  regenerate fixtures for `commit_detail_response_*` (uses
  CommitRow), `diff_response`, `plan_detail_response` (uses
  TimelineEvent + CommitRow).
- `crates/trinity-core/tests/round_trip.rs` — drop the
  `DiffLineKind` wire-string round-trip tests.
- `src/disk_snapshot.rs` — `apply_commit` constructs
  `PlanTimelineEvent` variants instead of setting `kind` +
  optional `gate`.
- `src/responses.rs` — `build_commits_array` / `build_timeline`
  rewritten to construct tagged variants; any internal
  pattern-matching on `event.kind` / `row.kind` becomes
  pattern-matching on the variant.
- `src/projection.rs` — `event_for*`,
  `latest_reviewable_commit_for`,
  `latest_reviewable_commit_gate_for`,
  `all_plan_revisions`, `all_implementation_commits`,
  `last_activity_ts_for`, `frozen_at` etc. all walk the timeline
  and read `e.kind` / `e.gate`. Each becomes a variant match.
- `src/diff_parser.rs` — `parse_diff` constructs `DiffLine`
  variants instead of building
  `DiffLine { kind, content, old_lineno: Some(...), ... }`.
- `frontend/src/components/structured_diff.rs` and any other
  view that pattern-matches on `DiffLine.kind` — switch to
  matching on the variant (no Option-unwrap on the linenos).
- `frontend/src/components/timeline.rs` and other consumers
  of timeline rows — switch from `if matches!(event.kind, ...)`
  to variant matching.
- `tests/end_to_end.rs` — any test that fishes for
  `commits[].kind` as a string keeps working (serde emits
  `"kind":"plan_only"` either way), but tests reading
  `commits[].gate` need to handle that the field only exists
  inside reviewable variants.

## Rules

- One discriminator per struct family. Where a tagged enum
  replaces a flat-struct-with-kind, the new enum's discriminator
  IS the kind; no parallel `kind` field on the variants.
- Variants in tagged enums carry exactly the fields valid for
  that kind. No `Option<Foo>` fields where Foo is uniformly
  populated within the variant.
- Closed-vocab `CommitKind` stays for places that genuinely need
  the value form (e.g. counting events by kind, filtering by
  reviewability). Tagged-enum membership doesn't lose this — a
  pattern match against the variant IS the kind check.

## Testing

- `cargo test --workspace --exclude trinity-frontend` —
  daemon + trinity-core tests.
- `cargo test -p trinity-frontend` — frontend pattern-match
  changes.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo fmt -- --check`.
- `cd frontend && trunk build --release` — wasm-side compiles
  (the structured-diff renderer is the main wasm consumer).
- Wire snapshot suite regenerated: confirm the new shapes
  (no `null` linenos on `DiffLine`, no `gate: null` on
  `PlanTimelineEvent`'s non-reviewable variants).
- Manual: bring up `just serve`, open a plan with a varied
  commit timeline (PlanOnly + CodeOnly + MultiPlan), spot-check
  the rendered timeline + a diff page.

## Acceptance criteria

- `CommitRow`, `PlanTimelineEvent`, `DiffLine` are all
  tagged enums (`#[serde(tag = "kind", rename_all = "snake_case")]`).
- No `Option<T>` field on the new enums whose Some-ness is
  determined by the variant — the data is structurally inside
  the variant or not at all.
- Wire-snapshot fixtures pass against the new shapes (after
  `UPDATE_SNAPSHOTS=1` regen).
- Frontend renders structured diffs and timelines correctly;
  matches the previous output.
- `vocab::DiffLineKind` is deleted.
- `vocab::CommitKind` stays (still useful as a value-type
  closed-vocab enum where the variant payload isn't relevant).
- All workspace tests pass.

## Non-goals

- Splitting `CommitKind` into a reviewable / non-reviewable
  sub-enum. The tagged enums keep the closed-vocab name on the
  wire (`"kind":"plan_only"`); the Rust-side enum has variants
  with payload. `CommitKind` itself stays as a separate
  value-type vocabulary enum.
- Reshaping `WaitTimeout` (the mild conditional-presence finding
  the survey flagged) — it's already inside the tagged
  `WaitForWorkResponse::Timeout` variant. The internal
  `no_active_plans` + `repo` conditional serialization is fine
  as-is.
- Changing `CommitKind` variant names. The wire-form snake_case
  strings (`plan_only`, `code_only`, etc.) are preserved.

## Trade-off honest record

The big call: this **breaks the wire shape** for three response
families (commit detail, plan detail, diff response). The shapes
are all also-typed in frontend code, so the Rust compiler catches
every consumer. No deserialize-time silent breakage on our side.

The smaller call: tagged-enum variants for the three reviewable
CommitKinds carry identical payload (`{sha, gate, feedback}`).
Could be modeled as `Reviewable { kind, sha, gate, feedback }`
sub-enum + `NonReviewable { kind, sha }`. Decided against:
keeping five variants makes the wire form symmetric with
`CommitKind` and avoids inventing a sub-vocabulary just to
deduplicate payload definitions. Repetition in the type def is
fine; the alternative obscures the kind on the wire.

`PlanTimelineEvent` is the most cross-cutting of the three —
it's daemon fold-state, not just a response DTO. Replacing it
touches disk_snapshot, projection, every test that builds a
timeline fixture. Worth it: pinning the gate/non-gate invariant
structurally removes a class of future bugs (e.g. the exact
"MultiPlan with gate" bug that
`responses.rs::divergence_tests` had to be written to pin).
