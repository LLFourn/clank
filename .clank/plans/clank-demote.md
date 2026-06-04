# clank-demote
# New subcommand to abort an in-flight plan: strip its commits AND save the plan body back to the queue

## Problem

Today there are two terminal states for an in-flight plan:
- **`clank finish <plan>`**: the happy path. Plan ships, commits stay in history (modulo `--purge` to strip clank artifacts).
- **`clank purge <plan>` then manual delete**: removes clank artifacts but PRESERVES the source-code commits. There's no clean way to say "this plan was a wrong direction; rewind it AND re-queue the description so I can try again later."

The gap is the "abort, re-queue" path. Without it, the only way to abandon an in-flight plan is to:
1. Manually `git reset --hard <intro^>` to drop commits.
2. Manually move the plan file back into `.clank/queue/`.
3. Renumber the queue priority by hand.
4. Hope the rest of the repo is clean.

That's error-prone and easy to half-do.

## Verified before promotion (audit 2026-06-05)

- **`RewriteDisposition::Drop` already exists** as a primitive in `crates/core/src/api.rs:163-167`. The rewrite engine at `cli/rewrite.rs:354` handles the Drop branch: the commit is excluded from the rewritten chain. So demote can compose the existing engine, not write a new one.
- **`clank purge`'s preview classifies commits per `RewriteDisposition`**:
  - `Drop`: commit's only content was `.clank/<plan>.md` paths — stripping leaves an empty commit, so drop it entirely.
  - `Rewrite`: commit touched plan files AND other content — strip the plan parts, keep the rest.
  - `KeepVerbatim`: foreign commit (not attributed to this plan) interleaved in the range.
- **Feedback files are NOT tracked in git** (verified by `.gitignore`: `.clank/*` with carve-outs only for `.clank/plans/` and `.clank/finished/`; `.clank/agents/` is local-only). So they don't appear in commit content; the rewrite engine doesn't see them. Orphan cleanup must happen as a filesystem-level pass alongside the rewrite, not as part of the rewrite itself.
- **`clank purge` already supports `--into-branch`, `--dry`, `--yes`, `--allow-rewrite-protected`.** Demote should inherit the same flags with the same semantics.

These findings collapse the plan's load-bearing logic into a small surface:

1. Demote's safety check reuses `clank purge`'s preview output. The check is simply "any `Rewrite` or `KeepVerbatim` disposition in the range?" — those are exactly the commits that have "real work" demote would lose.
2. With `--force`, demote uses a different disposition strategy: transform every commit in the range to `Drop` regardless. The rewrite engine then drops the whole range.
3. Orphaned feedback files: walk `.clank/agents/*/feedback/` after the rewrite succeeds; delete files whose key SHA is in the dropped set.
4. Plan-body save: read `.clank/plans/<plan>.md` from the working tree (or from the latest-reviewable tree if the working tree is dirty) BEFORE the rewrite; write to `.clank/queue/<NNN>-<plan>.md` or `.clank/stubs/<plan>.md`.

## Approach

New subcommand: `clank demote <plan> [--priority <N>] [--stub] [--force] [--into-branch <name>] [--dry] [--yes] [--allow-rewrite-protected]`

### Semantics

1. **Identify the plan's commit range** using the existing `build_rewrite_preview` for `<plan>`. Range is `[intro_sha, latest_reviewable]` per the existing plan-attribution logic.

2. **Safety check (tiered by danger level)**. Examine each commit's `RewriteDisposition` in the preview:
   - **All `Drop`** → safe to demote. The plan only contains plan-body edits.
   - **Any `Rewrite`** → at least one commit has non-plan content (user's own code-touching commits) that demote would lose. Refuse with a diagnostic naming the offending SHAs + their non-plan paths (from the preview's `strip_paths` field). `--force` overrides: the user explicitly opted in to losing their code changes.
   - **Any `KeepVerbatim`** → foreign commit interleaved in the plan's range. Refuse **unconditionally**, regardless of `--force`. Diagnostic: "foreign commit <SHA> is interleaved in this plan's range — demote can't safely drop someone else's work. Resolve via `git rebase -i` or coordinate with the foreign-commit author."

   Rationale for the two-tier policy (ruthless review of 91dc0b4): `Rewrite`-bypass via `--force` is user-recoverable — they wrote the code, they can rewrite it. `KeepVerbatim`-bypass would drop someone else's commit, which is a coordination event, not a flag-flip. Option C from the review: refuse foreign-commit drops entirely. The single `--force` flag is sufficient because it only ever applies to Rewrite cases now.

3. **Pre-rewrite checks (in order)**. Demote must be transactional: NOTHING on the filesystem changes until the rewrite has succeeded (codex review of 9bb7d0d). All checks happen first; in-memory state is staged but not written.
   - **Working-tree-dirty refusal**: if `.clank/plans/<plan>.md` in the working tree differs from HEAD's version, demote errors out. Diagnostic: "commit your plan-body changes or `git stash` before demoting." Rationale: silently choosing HEAD-or-working-tree would lose user edits one direction or the other; refusing matches clank's "fail closed on risky implicit state" pattern (ruthless review of 91dc0b4).
   - **Read plan body into memory** from HEAD's tree. Never reads the working tree (the dirty-refusal already ensured they match).
   - **Determine target path** for the saved body:
     - **Default**: `.clank/queue/<NNN>-<plan>.md` where `<NNN>` is `--priority` (default 500).
     - **`--stub`**: `.clank/stubs/<plan>.md`.
   - **Collision pre-check**: if the target file already exists, error out (don't clobber). Detected NOW, not after rewrite.

4. **Rewrite**: invoke the existing rewrite engine with all-Drop dispositions for the plan range. Foreign commits NOT in the range stay `KeepVerbatim`. The engine produces a new chain that excludes the plan's commits entirely. If the engine returns an error (protected-branch refusal, branch-name collision under `--into-branch`, plumbing error, stale ref) → demote bails BEFORE any post-rewrite writes happen. No queue/stub file. No orphan cleanup. Filesystem matches pre-demote state.

5. **Post-rewrite (only on success)**:
   a. **Write the plan body** to the target path determined in step 3.
   b. **Clean orphaned feedback files**: walk `.clank/agents/*/feedback/`; for each file whose name matches a dropped SHA (or maps to a dropped commit via the existing rewire indexing), delete it. Print a one-line summary of how many files were removed per agent.
   c. Print success summary: dropped SHA count, queue/stub target written, orphan-feedback count.

6. **`--into-branch` semantics — preview-only path**. When `--into-branch <name>` is set, the rewrite engine writes the rewritten chain to a fresh branch and leaves HEAD on master untouched. In that case:
   - The plan body is NOT written to the queue/stub.
   - The orphaned feedback is NOT cleaned up.
   - Print: "rewritten chain at `<name>`. To complete the demote: switch to `<name>` and run `clank demote <plan>` (without `--into-branch`)."
   Rationale (codex review of 9bb7d0d): `--into-branch` is a preview/safety mechanism for the rewrite. Writing the queue file or cleaning feedback while master still has `.clank/plans/<plan>.md` active would leave the repo in a contradictory state (one active plan + one queued duplicate). The full demote semantics only fire on the in-place path.

7. **Branch safety**: inherits `clank purge`'s protections. `--into-branch` refuses if `<name>` already exists. Refuses to rewrite protected branches in-place without `--allow-rewrite-protected`.

### Implementation note

Most of this composes existing code:
- Reuse `build_rewrite_preview` from `preview.rs`.
- Reuse the rewrite engine in `cli/rewrite.rs` — same RewriteOpts, just override the dispositions to all-Drop after the safety check passes.
- Reuse `clank rewire`-style feedback walking for the orphan cleanup.
- Reuse `clank purge`'s flag definitions for the inherited flags.

The genuinely new code is the safety-check classification + the plan-body save. Both are small.

### Flag summary

| Flag | Purpose |
|---|---|
| `<plan>` | Plan stem. Same parsing as `clank purge`. |
| `--priority <N>` | Queue priority for the re-queued plan body (default 500). Ignored with `--stub`. |
| `--stub` | Write to `.clank/stubs/<plan>.md` instead of `.clank/queue/`. |
| `--force` | Allow demote when the range has `Rewrite` dispositions (your own code-touching commits). Has NO effect on `KeepVerbatim` (foreign commits) — those refuse unconditionally. |
| `--into-branch <name>` | Preview-only: write rewritten chain to a fresh branch and leave master + queue/stub + feedback untouched. To complete the demote, the user switches to `<name>` and re-runs without `--into-branch`. Refuses if `<name>` already exists. Composes with `--dry`: prints the intended branch name without creating it. |
| `--dry` | Print the planned drop + safety check result + queue-write target + orphan feedback count; exit 0. Composes with `--into-branch`: shows the branch that would be created. |
| `--yes` | Skip interactive confirmation. |
| `--allow-rewrite-protected` | Inherited from purge. |

### Implementation note

The bulk of this is reusing `clank purge`'s rewrite engine — the engine already supports "drop these SHAs from history" as a primitive. The plan-specific safety check (refuse if non-plan commits) is the new logic.

The "save plan body before rewrite" is a one-line filesystem op; do it BEFORE invoking the rewrite so a failed rewrite doesn't leave the plan body in limbo.

## Out of scope

- Demoting a finalized plan. Once `clank finish` has run, the plan is in `.clank/finished/`; demoting that requires a different undo story (resurrection). Separate plan if needed.
- Renumbering existing queue items to make room for the re-queued plan. The new entry goes at `<priority>-<plan>.md`; if that collides, error (let the user pick a different priority).
- Recovering feedback from dropped SHAs. The plan-archival decision above handles this.
- Multi-plan demote. One plan at a time; users with multiple in-flight plans run multiple invocations.

## Acceptance

- `clank demote <plan>` on a plan whose commits ONLY touch `.clank/plans/<plan>.md` (all `Drop` dispositions in the preview) succeeds without `--force`. After: HEAD is at `<intro>^`, the plan body lives at `.clank/queue/500-<plan>.md`, the per-plan feedback files have been removed, and `git log` shows no trace of the dropped SHAs.
- `clank demote <plan>` on a plan with at least one `Rewrite` disposition (mixed plan+code commits) errors out naming the offending SHAs and their non-plan paths. No filesystem changes.
- `clank demote <plan>` on a plan with at least one `KeepVerbatim` disposition (foreign commit interleaved) errors out naming the foreign SHA + suggesting `git rebase -i` or coordination with the foreign-commit author. No filesystem changes.
- `clank demote <plan> --force` on the `Rewrite` error case drops the commits anyway (user opted in to losing their code).
- `clank demote <plan> --force` on the `KeepVerbatim` error case STILL refuses — the foreign-commit refusal is unconditional (ruthless review of 91dc0b4: dropping someone else's commit is a coordination event, not a flag-flip).
- `.clank/plans/<plan>.md` in the working tree differs from HEAD's version → demote errors out naming the dirty file, no filesystem changes. User commits/stashes before re-running.
- `--into-branch <existing-name>` errors out with "branch already exists". No filesystem changes — no queue/stub file, no feedback removed, original branch heads unchanged.
- **Transactional rollback (codex review of 9bb7d0d)**: when the rewrite engine fails for ANY reason after pre-checks pass (protected-branch refusal without `--allow-rewrite-protected`, dirty non-plan worktree, stale ref, plumbing error, branch collision), the queue/stub file is NOT written and orphan feedback is NOT cleaned. Filesystem matches pre-demote state. The user retries demote after fixing the underlying issue.
- `--into-branch <name> --dry` prints "would write rewritten chain to `<name>`" without creating the branch.
- `clank demote <plan> --stub` writes the plan body to `.clank/stubs/<plan>.md` instead of the queue.
- `clank demote <plan> --priority 100` puts the queue file at `.clank/queue/100-<plan>.md`.
- `clank demote <plan> --dry` prints the plan body's intended target, the per-commit disposition table, the orphan-feedback count, and exits 0 with no filesystem changes.
- `clank demote <plan> --into-branch <name>` writes the rewritten chain to a fresh branch; HEAD is unchanged; **no queue/stub file is written**; **no orphan feedback is cleaned**; stdout prints the completion-recipe ("switch to `<name>` and re-run without `--into-branch`").
- Target-collision: a pre-existing `.clank/queue/<NNN>-<plan>.md` (or `.clank/stubs/<plan>.md` with `--stub`) causes demote to error before any rewrite. Filesystem unchanged.
- After demote, `clank status` shows the plan back in the queue and no longer in `.clank/plans/`.
- `cargo test --workspace` passes.

## Tests

A new `crates/cli/tests/demote_integration.rs` (mirrors the existing rewrite/purge integration shape):

- `demote_plan_only_commits_succeeds_without_force`: plan with all-Drop dispositions; assert success + queue entry written + head moved + orphaned feedback removed.
- `demote_plan_with_code_commits_requires_force`: setup with `src/foo.rs` modified in a plan commit (Rewrite disposition); assert error names the SHA + filesystem unchanged.
- `demote_plan_with_foreign_commits_refuses_unconditionally`: foreign commit interleaved between intro and HEAD (KeepVerbatim); assert error names the foreign SHA + suggests `git rebase -i` + filesystem unchanged.
- `demote_force_drops_mixed_commits`: Rewrite-case + `--force`; assert drop succeeds and the code changes are gone (the user opted in).
- `demote_force_does_not_bypass_foreign_refusal`: KeepVerbatim-case + `--force`; assert error STILL fires + filesystem unchanged. Locks in the unconditional refusal.
- `demote_dirty_plan_file_errors`: working-tree-modified `.clank/plans/<plan>.md`; assert error names the dirty file, no rewrite happened.
- `demote_stub_writes_to_stubs_dir`: `--stub` lands at `.clank/stubs/<plan>.md`.
- `demote_priority_writes_to_queue_with_priority`: `--priority 100` lands at `.clank/queue/100-<plan>.md`.
- `demote_dry_no_changes`: `--dry` prints intent (per-commit disposition + queue target + orphan count) + exits 0 without filesystem changes.
- `demote_into_branch_does_not_touch_head`: `--into-branch <name>` leaves master alone, writes the chain to `<name>`. NO queue file written. NO feedback removed. stdout names the completion-recipe.
- `demote_into_branch_collision_errors`: pre-create `<name>`; assert demote refuses + filesystem unchanged (no queue file, no feedback removed).
- `demote_into_branch_with_dry_does_not_create_branch`: `--into-branch <name> --dry` reports intent + leaves `<name>` non-existent + no queue file written.
- `demote_protected_branch_refusal_leaves_no_partial_state`: protected branch + no `--allow-rewrite-protected`; assert error + queue file NOT written + feedback NOT cleaned. Locks in the transactional rollback for the most common failure mode.
- `demote_target_collision_detected_before_rewrite`: pre-populate `.clank/queue/500-<plan>.md`; assert error fires BEFORE git history is touched (HEAD unchanged, no orphaned partial-rewrite state).
- `demote_priority_collision_errors`: pre-populate `.clank/queue/500-<plan>.md`; assert demote errors out before any rewrite.
- `demote_stub_collision_errors`: pre-populate `.clank/stubs/<plan>.md`; assert demote --stub errors out.
- `demote_orphaned_feedback_removed`: per-plan feedback files (`.clank/agents/*/feedback/<dropped-sha>.md`) are gone after a successful demote.

## Related history

- `clank purge` (existing): strips `.clank/` artifacts from history while preserving source-code commits.
- `clank finish` (existing): the happy-path terminal state.
- `clank queue add` (existing): the inverse of demote — promote a queue item to active.

Demote sits between purge and reset-by-hand: it does the destructive history rewrite purge does, but also drops the source commits AND saves the plan body for re-attempt.
