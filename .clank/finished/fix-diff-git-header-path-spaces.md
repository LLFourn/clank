# fix-diff-git-header-path-spaces
# Delete the unused diff parser surface in clank-cli

## Rescope notice

This plan was originally "fix `parse_diff_git` for paths with spaces". Investigation during the planning phase (block `dead-code-question`, user resolution: option B) found that `parse_diff`, `parse_diff_git`, `diff_two_blobs`, and `show_commit` have **zero callers anywhere in the workspace** — not in clank-cli binaries, not in clank-core, not in integration tests, not in the html / commit-detail / review-UI paths. The bug being fixed is in code nothing executes.

Replacing the fix with a deletion. Title kept for traceability with the queue + reviewer history.

## Problem

The clank-cli crate exports a diff-parsing surface that is dead code:

- `crates/cli/src/diff_parser.rs` — `pub fn parse_diff(raw: &str) -> Vec<FileDiff>` plus its internal helper `parse_diff_git`, `parse_hunk_starts`, `strip_git_prefix`, `is_always_folded`, and the test module.
- `crates/cli/src/git_io.rs:155` — `pub async fn diff_two_blobs(...)`, the producer that was intended to feed `parse_diff`.
- `crates/cli/src/git_io.rs:171` — `pub async fn show_commit(...)`, also intended to feed `parse_diff`.
- `crates/cli/src/lib.rs:2` — `pub mod diff_parser;` declaration that exports the module externally.

Verified via grep across `crates/`, `tests/`, and binary entry points: none of these symbols are referenced anywhere except in `diff_parser.rs`'s own test functions. They form a self-contained, untested-in-production, possibly-buggy island.

Symptoms (motivated the original plan, still real if anything ever calls these):
- `parse_diff_git` mis-splits paths with whitespace.
- `parse_diff` lacks producer-prefix contract; mnemonic-prefix configs would break it.

But fixing both of those costs days of work (per the prior plan revisions) to harden code with no execution path.

## Approach

Delete the dead code in one focused change.

### Files / lines to remove

1. **Delete `crates/cli/src/diff_parser.rs`** entirely. Includes:
   - `parse_diff`, `parse_diff_git`, `parse_hunk_starts`, `parse_start`, `is_always_folded`, `strip_git_prefix`, `finalize`.
   - The three `#[test]` functions in the embedded test module.
2. **Delete the `pub mod diff_parser;` line** in `crates/cli/src/lib.rs:2`.
3. **Delete `diff_two_blobs`** in `crates/cli/src/git_io.rs:155-169` (the function body + the doc comment block above it that references `parse_diff` / "the daemon").
4. **Delete `show_commit`** in `crates/cli/src/git_io.rs:171-173`.
5. **Update the stale reference comment** in `git_io.rs:466` that mentions `show_commit` (since the function it cites will be gone — either delete the comment or rewrite it without the dead reference).

### What is NOT deleted

The `FileDiff`, `FileDiffMode`, `DiffHunk`, `DiffLine` types in `crates/core/src/api.rs` stay. Rationale:

- They're `pub` in `clank_core::api` and carry `Serialize`/`Deserialize` derives.
- `crates/core/tests/round_trip.rs` has serde round-trip tests for `DiffLine`, indicating an intentional public wire-format contract.
- Even with `diff_parser` gone, they remain valid public API for any downstream consumer (an external tool, a future daemon endpoint, a re-implemented parser).
- Removing them is a separate concern; this plan stays scoped to clank-cli's dead island.

## Verification

1. `cargo build -p clank` succeeds with no errors.
2. `cargo test -p clank` passes (we're removing test functions; no other tests should reference the deleted symbols).
3. `cargo clippy -p clank` produces no new dead-code warnings (we should be REMOVING warnings, not adding them — but verify nothing unexpected was depending on the dead surface as a re-export).
4. `cargo build --workspace` and `cargo test --workspace` pass — confirms clank-core's `FileDiff` family still compiles and round-trip tests still run.

## Out of scope

- Deletion of `FileDiff` / `FileDiffMode` / `DiffHunk` / `DiffLine` from `clank-core::api` — left as potentially-public API; would need its own plan.
- Whether `parse_diff` should be re-introduced when there's a real consumer — leave that to a future implementation plan with a concrete user (e.g., html commit-detail rendering, a review-UI route, etc.).
- The `core.quotePath`, prefix-pin, and TAB-delimiter concerns from the prior plan revisions — moot since the code being concerned about is gone. The hard-won design notes in this file's git history (commits b834f77 / 040cc7d) remain available if anyone re-implements.

## Acceptance

- `crates/cli/src/diff_parser.rs` does not exist.
- `crates/cli/src/lib.rs` does not declare `pub mod diff_parser`.
- `diff_two_blobs` and `show_commit` are not defined in `crates/cli/src/git_io.rs`.
- `cargo build --workspace && cargo test --workspace` succeeds.
- No references to the deleted symbols remain in `crates/` (verified via `grep -r`).
