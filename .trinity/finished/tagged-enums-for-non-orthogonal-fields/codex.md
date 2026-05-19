APPROVE

Reviewed the full series (90ceed3 plan intro → ef45532 plan
revision → 496f18d Phase 1 → ed2fa97 Phase 2 → ca93892 Phase 3 →
4febb17 round-trip coverage followup).

The three non-orthogonal-field antipatterns the survey identified
are now structural in the type system:

- **`DiffLine`** (`crates/trinity-core/src/api.rs`) is a four-
  variant tagged enum. `Insert` carries only `new_lineno`,
  `Delete` only `old_lineno`, `Context` carries both, `Meta`
  neither. Wire shape stops carrying `"old_lineno":null` /
  `"new_lineno":null` placeholders on thousands of lines per
  diff. `vocab::DiffLineKind` is deleted (subsumed by the
  discriminator).

- **`CommitRow`** (`crates/trinity-core/src/api.rs`) is a three-
  variant tagged enum covering only the reviewable kinds
  (`PlanOnly`, `CodeOnly`, `Mixed`) — matching the
  `build_commits_array` filter that's always been in place.
  `gate` is non-Option inside every variant. Accessor methods
  (`sha`, `kind`, `gate`, `feedback`) keep call sites readable.

- **`PlanTimelineEvent`** (`crates/trinity-core/src/model.rs`)
  is a five-variant tagged enum. Reviewable variants carry a
  structurally-non-Option gate; `MultiPlan` and `Finalize`
  cannot carry one at all. The `responses.rs::divergence_tests`
  block — which pinned the "MultiPlan-with-gate gets the gate
  ignored on the wire" invariant — was deleted because that
  invariant is now syntactically un-constructible. The
  `debug_assert!` in `build_timeline` it worked around went with
  it.

Accessor methods on `PlanTimelineEvent` (`sha`, `author_ts`,
`subject`, `kind`, `is_reviewable`, `gate`, `gate_mut`) absorb
the "I just want the value form" reads across `projection.rs`,
`responses.rs`, `runtime.rs`, `repo_state.rs`, and tests —
saving repetitive five-arm matches in projection code.

Writer-side fixups in `runtime.rs`: the old
`event.gate.get_or_insert_with(...)` and `event.gate = Some(...)`
patterns became `event.gate_mut().expect("is_reviewable() implies
a gate")`, guarded by the existing `is_reviewable()` early-return.

Wire-shape impact:

- `DiffLine` rows shrink (no `null` lineno placeholders).
- `CommitRow` rows now embed the kind via `tag = "kind"`; the
  reviewable-only filter is unchanged.
- `PlanTimelineEvent` rows likewise; non-reviewable variants no
  longer ship a `null` gate field.

Test coverage: 12 new per-variant round-trip tests in
`crates/trinity-core/tests/round_trip.rs` pin each variant's wire
shape (including absence of the structurally-absent fields).
Schema snapshots regenerated for `commit_detail_response_*`,
`diff_response`, and `plan_detail_response`.

Verification commands all green at the close of the plan:
- `cargo build --workspace --all-targets`
- `cargo test --workspace --exclude trinity-frontend`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cd frontend && trunk build --release`
- `cargo test -p trinity-core --test wire_snapshots`
- `cargo test -p trinity-core --test round_trip`

Approved as-is.
