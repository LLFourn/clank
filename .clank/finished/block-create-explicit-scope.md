# block-create-explicit-scope
# Require `clank block create` to be explicit about scope (`--plan <name>` OR `--all`)

## Problem

`clank block create <name> -m "<q>"` defaults to repo scope today (verified at `crates/cli/src/cli/block.rs` in `run_create`: if `args.plan` is None, the block file lands in `agents/<author>/blocks/` directly, which `wfw::check_blocks` treats as `plan: None` → `suppress_all = true`).

The repo-scope default is over-blocking by accident:
- A block almost always relates to a specific in-flight plan.
- The cost of forgetting `--plan <name>` is silently suppressing every wfw item for the agent across every plan and every queue item.
- The cost of explicitly requiring scope is one extra flag the user has to type — small and self-correcting (the error tells them what to do).

## Verified before promotion

- `BlockCreateArgs` has `pub plan: Option<String>` (`block.rs:run_create` line 1-30); no `--all` flag exists. Adding one + flipping `plan` to required-unless-`--all` is the structural change.
- `wfw::check_blocks` reads `b.plan == None` → `suppress_all = true`. Once `clank block create` refuses to write a no-scope block, that branch can only be reached by hand-edited block files. No need to teach wfw to reject it — the producer side is the right gate.
- No existing tests pin "block create with no flags works." The blocking tests pin specific positive cases; they'll need a `--plan` flag added or migrate to `--all`.

## Approach

1. **CLI surface**: add `--all` flag to `BlockCreateArgs`. Require exactly one of `{--plan <name>, --all}`. If neither → error with a helpful message naming both options. If both → error (mutually exclusive, same shape as `clank purge --all` vs `<plan>`).
2. **`run_create` body**: branch on the flag:
   - `--plan <name>`: existing path. Writes to `agents/<author>/blocks/<plan>/<name>.md`.
   - `--all`: writes to `agents/<author>/blocks/<name>.md` (the legacy repo-scope path). Diagnostic on stderr names it "REPO-WIDE BLOCK".
3. **Update embedded SKILL.md content** at `crates/cli/src/cli/setup_assets/claude_skill.md` AND `crates/cli/src/cli/setup_assets/codex_skill.md`. Both files currently embed the old invocation pattern `clank block create <name> -m "question"` with no scope flag (verified by grep). After this plan ships, existing skill content would point agents at a now-erroring command shape. Update both to show `clank block create <name> --plan <plan-stem> -m "question"` as the primary recipe, with a one-line note that `--all` exists for genuinely repo-wide blocks.
4. **No on-disk schema change**: the `agents/<author>/blocks/<plan>/<name>.md` vs `agents/<author>/blocks/<name>.md` distinction stays as-is. Only the CLI's input validation changes.
5. **Doctor unchanged**: doctor doesn't surface block-scope concerns and shouldn't.
6. **Migration path for installed skills**: the `install_skill` refuse-if-drifted contract means existing agents will hit a "drifted from embedded content — run `clank setup --force`" doctor warning until they re-run setup. That's acceptable — same friction pattern as every other skill update.

## Out of scope

- Migrating existing repo-scope blocks. Old hand-edited blocks continue to work via the existing on-disk schema.
- Replacing `--all` with a confirmation prompt for repo-scope. The flag itself is the explicit consent.

## Acceptance

- `clank block create foo -m "q"` (no scope) errors out, naming `--plan <name>` and `--all` as the two ways to proceed. Exit non-zero. No file written.
- `clank block create foo --plan replace-git-io-with-gix -m "q"` writes to `.clank/agents/<author>/blocks/replace-git-io-with-gix/foo.md` (current behavior under that flag).
- `clank block create foo --all -m "q"` writes to `.clank/agents/<author>/blocks/foo.md` AND prints a `REPO-WIDE BLOCK` notice on stderr.
- `clank block create foo --plan X --all -m "q"` errors out with the mutually-exclusive message.
- `cargo test --workspace` passes.

## Tests

- `block_create_no_scope_errors`: assert non-zero exit + diagnostic mentions both flag names.
- `block_create_with_plan`: assert path lands under `blocks/<plan>/`.
- `block_create_all_writes_repo_scope`: assert path lands directly under `blocks/` + REPO-WIDE notice on stderr.
- `block_create_plan_and_all_errors`: mutually-exclusive enforcement.
- **Skill-asset regression**: a unit test in `setup.rs`'s test module asserts both `CLAUDE_SKILL_BODY` and `CODEX_SKILL_BODY` contain `clank block create` AND `--plan` (not the bare invocation). Locks in the asset update so future skill edits don't re-introduce the bare form.

Tests live in a new `crates/cli/tests/block_create_scope_integration.rs` (mirrors the existing `setup_codex_rule_integration.rs` shape — temp HOME, spawn the clank binary, assert exit + file state).
