# dry-run-executes-rendered-program

## Summary

Make `clank purge --amend --dry` execute the same typed program that
the live path uses. Today `--amend --dry` is a hand-rolled preview
(lines 314-322 in `purge.rs`) while the live path inline-builds its
own git-rm + git-commit-amend sequence. The two can drift.

The rewrite engine (`rewrite.rs`) already has this property for the
standard purge/squash paths — `build_plan()` is called once and
either rendered or executed. `--amend` is the last mode that bypasses
the engine entirely with inline git plumbing.

Narrow scope: fix only the `--amend` path. The standard rewrite
engine is already sound. Squash has a smaller gap (shared inputs,
separate render/execute) that can be addressed as a follow-up.

## The gap

`run_amend` (`purge.rs:152-354`) does everything inline:

1. Resolves HEAD, validates it's a finalize commit.
2. Computes `strip_paths` via `git ls-tree`.
3. If `--dry`: renders a hand-formatted preview.
4. If live: runs `git rm --cached` + `git commit --amend`.

Steps 3 and 4 share the `strip_paths` vec but otherwise have no
structural coupling — they're siblings, not stages of one program.

## Fix

### Typed program

```rust
pub struct AmendProgram {
    pub head_sha: String,
    pub strip_paths: Vec<String>,
}
```

### Two-stage flow

```rust
let program = build_amend_program(repo, plan_key)?;
if dry {
    render_amend_dry(&program);
} else {
    execute_amend(&program)?;
}
```

`build_amend_program` replaces the inline resolution + validation
in `run_amend` (HEAD validation, strip_paths computation). It
returns the program or errors on pre-flight failures.

`render_amend_dry` replaces the hand-formatted preview at lines
314-322. It renders from the program struct, not from loose locals.

`execute_amend` replaces the inline git-rm + git-commit-amend
sequence. It consumes the same program the render produced. Before
mutating, it re-reads HEAD and bails if it differs from
`program.head_sha` — a CAS guard against concurrent HEAD movement
between build and execute.

### Testable invariant

A test builds the program once and asserts that:
- `render_amend_dry(&program)` produces a parseable preview
  mentioning every `strip_path`.
- `execute_amend(&program)` modifies HEAD so the stripped paths
  are absent and the rest are preserved.

Both consume the same `AmendProgram` value — drift is a type error.

## Implementation surface

All in `crates/cli/src/cli/purge.rs`:

- New `AmendProgram` struct (pub for testing).
- New `build_amend_program(repo, plan_key) -> Result<AmendProgram>`.
  Absorbs the HEAD validation, ls-tree, and strip_paths computation
  from `run_amend`.
- New `render_amend_dry(program)` (replaces lines 314-322).
- New `execute_amend(repo, program)` (replaces lines 334-353).
- `run_amend` shrinks to: pre-flight checks (dirty worktree,
  protected branch), `build_amend_program`, confirm, branch on
  `--dry` → render vs execute.

No changes to `rewrite.rs` or the standard purge/squash path.

## Tests

1. **Dry and live share the same program**: build `AmendProgram` in
   a test repo with a finalize commit. Assert both `render_amend_dry`
   and `execute_amend` accept the same struct value.
2. **Render output mentions every strip_path**: golden-style check
   that the dry output lists every path from the program.
3. **Execute strips paths from HEAD**: after `execute_amend`,
   `git ls-tree HEAD` no longer shows the stripped paths.
4. **Stale HEAD bails**: move HEAD between `build_amend_program` and
   `execute_amend`; assert the execute bails with a clear message
   rather than amending the wrong commit.
5. **Existing `purge --amend` integration tests** (if any) still
   pass — behavior is preserved, only the internal structure changed.

## Acceptance criteria

- `run_amend` no longer contains inline git-plumbing. All mutation
  flows through `execute_amend(repo, &program)`.
- `--dry` renders from the same `AmendProgram` struct that live
  executes.
- A test verifies dry and live consume the same typed value.
- `execute_amend` re-reads HEAD and bails if it differs from
  `program.head_sha` (CAS guard against concurrent HEAD movement).
- No changes to the rewrite engine or the standard purge path.

## Out of scope

- Squash dry/live unification (smaller gap, separate plan).
- `clank finish --purge` / `clank finish --squash` — they delegate
  to the rewrite engine which already shares `ExecutionPlan`.
- Machine-parseable `--dry --format json` output.
