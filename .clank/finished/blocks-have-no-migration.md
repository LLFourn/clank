# blocks-have-no-migration

Invariants: I2.

## Why

`a-block-belongs-to-the-repo` moved blocks from `.clank/agents/<label>/{blocks,unblocks}` to `.clank/blocks` and `.clank/unblocks`. It did this with a journaled migration (`crates/cli/src/cli/block/migration.rs`) that runs inside `block::lock()` on every read.

- One orphan legacy `unblocks/*.md` makes every reader refuse.
- The real questions disappear behind a synthetic "needs repair" entry that pauses review indefinitely.
- No command repairs it.
- The collision report goes to stderr nobody sees.
- `block-migration.json` is reread on every lock.

The user's decision: no migration. The new layout is a breaking change.

## Build

1. Delete `block/migration.rs` and every caller. `lock()` takes the lock and nothing else.
2. Delete every reader of the per-agent layout:
   - per-agent `blocks/`, `unblocks/` and `blocks.lock`
   - `block-migration.json`
   - the synthetic repair entry and its wake/status rendering
3. Stray legacy files are ignored, never read.
4. CHANGELOG, under Breaking: blocks pending in the old per-agent layout are not carried over. Re-ask them; the old directories can be deleted.
5. Fix the README text that describes block migration.

6. Update I2 in `docs/invariants.md`: it holds for blocks once `lock()` only locks. Name the test below, and remove this plan from I2's `Plans`. If another read with side effects is found, it stays a target naming it.

## Acceptance

- A repo holding legacy per-agent block files, including an orphan answer, lists and answers repo-level blocks normally.
- No production code names `block-migration.json` or the per-agent block paths.
