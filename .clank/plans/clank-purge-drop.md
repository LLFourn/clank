# clank-purge-drop
# `clank purge <plan> --drop` — destructive variant that drops EVERY commit attributed to the plan, code and all, not just `.clank/` artifacts.

## Problem

lloyd 2026-06-05:

> add `clank purge <plan-name> --drop` which just deletes all commits in the plan by rewriting history -- unlike a normal purge it totally removes the plan AND the implementation.

Today `clank purge <plan>` strips ONLY the plan's `.clank/` artifacts from history (the plan body and finalize snapshot). Implementation commits — the code the agent wrote while the plan was active — survive the rewrite with their `.clank/` paths scrubbed away. That's the right default ("erase the audit trail, keep the work") but there's no clean way to say "this plan and everything it produced was a wrong direction; rewind both."

The closest existing path is `clank demote <plan> --force`: it DOES drop every plan-attributed commit, but it ALSO writes the plan body back to `.clank/queue/<NNN>-<plan>.md` (or `.clank/stubs/`) and cleans orphan feedback. That's the "abort, re-queue, retry later" path, not "this never happened."

The gap is the "totally remove" terminal state. `--drop` fills it. It is a strict subset of `clank demote --force`: same disposition transform, same safety stance, but no save-to-queue and no stub write.

## Verified before promotion (audit 2026-06-05)

- **`RewriteDisposition::Drop` is the engine primitive** at `crates/core/src/api.rs:163-167`. The rewrite engine handles `Drop` at `crates/cli/src/cli/rewrite.rs:476-480` (the parent chain skips the commit entirely). So `--drop` composes the same engine + manifest path that `purge` and `demote` already use, just with a different disposition transform.
- **Today's `clank purge` runs the preview through the engine verbatim** — `crates/cli/src/cli/purge.rs:67-87` (`run_single`) calls `build_rewrite_preview(..., include_finalize=true)` and passes the resulting `preview.commits` straight into `RewriteOpts`. The preview classifies commits per `RewriteDisposition::{Drop, Rewrite, KeepVerbatim}` (`crates/cli/src/preview.rs:184-206`). `--drop` is "transform every non-foreign step to `Drop` BEFORE invoking the engine" — a tiny pre-engine adjustment, not a new engine.
- **`clank demote --force` already implements the exact transform we want** at `crates/cli/src/cli/demote.rs:100-116` (the iterator over `preview.commits` that rewrites every non-foreign commit to `Drop`). The body is ~15 lines. `--drop` lifts it verbatim into purge and skips demote's downstream queue/stub write + orphan-feedback walk (`demote.rs:166-191`).
- **`safety_check` in demote** (`demote.rs:205-241`) implements the tiered policy: foreign commits refuse unconditionally; `Rewrite` non-foreign requires `--force`. The interesting question for purge is whether the safety check should fire under `--drop`. See Open questions.
- **Existing purge flags compose with the engine via `RewriteOpts`** (`crates/cli/src/cli/rewrite.rs:21-57`): `--into-branch`, `--dry`, `--allow-rewrite-protected`, `--yes`, `--squash`. `--drop` should inherit these as-is where the semantics make sense (most do) and refuse at parse time where they don't (squash).
- **PurgeArgs is in `crates/cli/src/cli/mod.rs:831-882`** — adding `--drop` is a single bool field with a docstring.
- **The all-Drop edge case is already handled** by the engine at `rewrite.rs:497-505`: when every step in the range Drops AND there's no `intro_parent` (the intro is the root commit), the engine errors with "cannot point a branch at 'nothing'". `--drop` inherits this — useful diagnostic to keep.

These findings collapse the plan's load-bearing logic into a small surface:

1. Add `--drop: bool` to `PurgeArgs`.
2. In `run_single` (the `--all` path is out of scope — see Out of scope), AFTER `build_rewrite_preview` and BEFORE `run_rewrite`, branch on `args.drop`: if set, run the demote-style safety check + transform every non-foreign disposition to `Drop`. If not set, today's behavior is unchanged.
3. Tighten the confirmation prompt under `--drop` so the operator knows code is going away, not just `.clank/` artifacts.

Everything else (engine, ref-update guard, protected-branch refusal, `--into-branch` preview semantics) is reused unchanged.

## Approach

### Phase 1: CLI surface

Add to `PurgeArgs` in `crates/cli/src/cli/mod.rs`:

```rust
/// Drop EVERY commit attributed to the plan, including
/// implementation code — not just `.clank/` artifacts. The plan
/// AND its work both vanish from history. Refuses foreign
/// commits unconditionally (same policy as `clank demote`).
/// Does NOT save the plan body anywhere — use `clank demote` if
/// you want to re-queue the plan for another attempt.
#[arg(long)]
pub drop: bool,
```

Parse-time refusals (in `purge::run`, alongside the existing `--all`/`--squash`/`--amend` checks):

- `--drop` + `--all`: refuse. `--all` already strips every `.clank/` path across all plans; adding "drop everything" semantics there would be a bulldozer, not a tool. Out of scope for v1.
- `--drop` + `--squash`: refuse. You can't squash what you're dropping; the combination is nonsensical.
- `--drop` + `--amend`: refuse. `--amend` rewrites only HEAD's tree; `--drop` rewrites the whole chain. The two flags model different operations.
- `--drop` without a plan argument (and not `--all`): allowed — same plan-resolve fallback as today (single in-flight plan).

### Phase 2: Disposition transform + safety check

In `run_single` (`crates/cli/src/cli/purge.rs:54-100`), after the existing `build_rewrite_preview` call and before `run_rewrite`, insert the transform. Shape (mirrors `demote.rs:100-116`):

```rust
let commits_for_engine: Vec<RewriteCommit> = if args.drop {
    drop_safety_check(&preview.commits)?;
    preview
        .commits
        .iter()
        .map(|c| if c.foreign {
            c.clone()
        } else {
            RewriteCommit {
                sha: c.sha.clone(),
                subject: c.subject.clone(),
                disposition: RewriteDisposition::Drop,
                foreign: false,
                strip_paths: c.strip_paths.clone(),
            }
        })
        .collect()
} else {
    preview.commits.clone()
};
```

Then feed `commits_for_engine` into `RewriteOpts.commits` instead of `preview.commits` directly.

**Safety check.** The simplest pin: copy demote's foreign-commit refusal verbatim, but DROP the `Rewrite`-requires-`--force` tier. Rationale: `--drop` IS the opt-in — the user typed `--drop` because they want everything gone, including their own code. A second `--force` flag would be ceremony for ceremony's sake. So the policy under `--drop`:

- **Any foreign commit in range → refuse unconditionally.** Same message demote uses ("foreign commit interleaved … `--force` does NOT bypass this … resolve via `git rebase -i` or coordinate"). Reword to mention `--drop` instead of demote.
- **Any `Rewrite` non-foreign → just drop it.** No second confirmation flag.

This is a meaningful divergence from demote and worth pinning at promote-time. Alternative: gate `Rewrite` non-foreign behind a separate `--force-drop-code` flag. See Open questions.

Implementation note: the safety check lives in `purge.rs` as a free function (`drop_safety_check`) rather than reaching into `demote.rs`. The shared structure across both call sites is the disposition iterator pattern, not a re-exported helper — copy is cheaper than coupling here.

### Phase 3: Confirmation prompt

Today's `confirm_single` (`purge.rs:428-438`) says:

> About to purge `<stem>` and write rewritten history to <target>. Continue? [y/N]

Under `--drop`, swap in a louder banner. Sketch:

> About to DROP all <N> commits attributed to `<stem>` and write rewritten history to <target>. This deletes the implementation code, not just `.clank/` artifacts. Continue? [y/N]

Where `<N>` is `preview.commits.iter().filter(|c| !c.foreign).count()` so the operator sees the actual blast radius. `--yes` still skips the prompt (script paths shouldn't lose access to `--drop` just because it's louder).

### Phase 4: `--into-branch` composition

Reuse the existing preview-only semantics today's `purge --into-branch` already has: write the rewritten chain to a fresh branch, leave the current branch untouched, no plan-body archive (purge doesn't archive anyway). Under `--drop --into-branch <name>`, the chain on `<name>` is "plan and impl both gone." This is the natural and useful safety check for a destructive op — operator can `git diff <current>..<name>` to inspect what's about to be lost.

This needs no special-case code in purge — the engine already handles `--into-branch` per-disposition. The only nuance is the confirmation prompt should reflect the target branch (existing `confirm_single` already does this).

### Phase 5: Wiring + tests

- Add `drop: false` to the test constructor in `purge.rs` and any other test that builds `PurgeArgs`.
- Add new tests per the Tests section.
- No new integration crate needed — the existing inline tests in `crates/cli/src/cli/purge.rs::tests` are the natural home, with the same `init_test_repo` + `write_file` + `head_sha` helpers already used by the `--amend` tests.

## Out of scope

- **`--drop --all`**. Semantically possible ("drop EVERY commit that touched `.clank/`") but practically a footgun: the all-plans range includes every plan's intro and every commit attributed to any plan, which on a long-lived repo is "essentially everything." Defer; if someone genuinely wants it, follow-up plan.
- **Saving the plan body anywhere under `--drop`**. That's `clank demote`. The whole point of `--drop` is "this didn't happen"; archiving the plan defeats the purpose. Document the relationship in `--help`.
- **Cleaning orphan feedback files under `--drop`**. Feedback files for the dropped SHAs become orphans, same as under `clank demote`. Demote's orphan-cleanup walk (`demote.rs:301-317`) is reusable code — but the `--drop` path probably should NOT clean them automatically. Rationale: `clank purge` today doesn't touch `.clank/agents/`, and surprising script-driven invocations with filesystem writes outside the rewrite is the kind of expansion that breeds bugs. Surface it as a hint at the end ("N orphan feedback files remain; run `clank rewire` or delete `.clank/agents/*/feedback/<sha>.md` to clean them up"). Pin at promote-time.
- **`--drop` on a finished plan**. `build_rewrite_preview` already supports finished plans (it re-folds to derive native SHAs — `preview.rs:323-359`). So mechanically this works. But a finished plan's intro→finalize range includes the finalize commit itself, which dropping completely scrubs the audit trail. Probably fine, but worth deciding: refuse with "use `clank unfinish` first" vs allow. Default to "allow" since `--drop` is already the destructive lane.
- **`--drop --amend` and `--drop --squash`**. Both refused at parse time. See Phase 1.
- **Renaming the flag**. `--drop` per Lloyd's explicit wording. Alternatives flagged in Open questions for the promotion review.

## Acceptance

- `clank purge <plan> --drop` on a plan with mixed plan-body + implementation commits succeeds (with confirmation), and `git log` afterwards shows zero commits attributed to `<plan>` — including the commits that today's `clank purge` would have rewritten to keep the code. The current branch is updated atomically via the engine's conditional `update-ref`.
- `clank purge <plan> --drop` on a plan whose range contains a foreign commit refuses with a message naming the foreign SHA and suggesting `git rebase -i` or coordination. No filesystem changes, no branch movement.
- `clank purge <plan> --drop --into-branch <name>` writes the dropped chain to `<name>` and leaves the current branch alone. `<name>` already existing is a refusal (inherited engine behavior).
- `clank purge <plan> --drop --dry` prints the rebase-todo preview with every plan-attributed commit shown as `drop`, no commits or refs created.
- `clank purge <plan> --drop` on a protected branch without `--allow-rewrite-protected` refuses (inherited engine behavior).
- `clank purge --drop --all` errors at parse time with a clear "not supported" diagnostic.
- `clank purge <plan> --drop --squash <msg>` errors at parse time.
- `clank purge <plan> --drop --amend` errors at parse time.
- `clank purge <plan> --drop` does NOT write anything under `.clank/queue/`, `.clank/stubs/`, or `.clank/agents/`. The plan body and its history are simply gone.
- The confirmation prompt under `--drop` explicitly mentions that code is being deleted, not just `.clank/` artifacts. `--yes` bypasses it as today.
- `cargo test --workspace` passes.

## Tests

Inline tests in `crates/cli/src/cli/purge.rs::tests` unless otherwise noted:

1. `drop_drops_plan_only_commits`: plan with all-Drop dispositions; `--drop` succeeds, branch tip moves to `intro^`, plan paths gone.
2. `drop_drops_mixed_plan_and_code_commits`: plan with Rewrite (mixed plan+code) dispositions; `--drop` drops them too, no `--force` needed. After: `src/foo.rs` modifications introduced in those commits are GONE from the resulting branch. (This is the test that proves `--drop` differs from today's `clank purge` — today's purge would keep `src/foo.rs`.)
3. `drop_refuses_foreign_commit_unconditionally`: foreign commit interleaved in plan range (KeepVerbatim, foreign=true); `--drop` errors naming the foreign SHA. No branch movement.
4. `drop_into_branch_preview_does_not_touch_current_branch`: `--drop --into-branch scrubbed`; `scrubbed` has the dropped chain, current branch (`work`/`main`) is unchanged.
5. `drop_dry_prints_planned_drops_without_touching_refs`: `--drop --dry` produces a rebase-todo where every plan-attributed step is `drop`, no refs created.
6. `drop_with_all_errors_at_parse_time`: `clank purge --drop --all`; error contains "not supported".
7. `drop_with_squash_errors_at_parse_time`: `clank purge <plan> --drop --squash "msg"`; error contains "mutually exclusive" or "not supported".
8. `drop_with_amend_errors_at_parse_time`: `clank purge <plan> --drop --amend`; error fires before any preview.
9. `drop_protected_branch_refusal_inherited`: `--drop` on `main` without `--allow-rewrite-protected`; engine refuses with the protected-branch message.
10. `drop_does_not_write_queue_or_stub`: after a successful `--drop`, assert `.clank/queue/`, `.clank/stubs/`, and `.clank/agents/*/feedback/` are unchanged (modulo orphan files if we go the leave-them route).
11. (Stretch, gate-able) `drop_unknown_plan_errors_clearly`: `clank purge does-not-exist --drop`; error names the plan and mentions it isn't active or finished.

## Related history

- **`clank-demote` (FINISHED, `clank-demote.md`)** — closest sibling. `--drop` lifts demote's disposition transform (`demote.rs:100-116`) and foreign-commit safety check (`demote.rs:205-241`) wholesale. Diverges by skipping the save-to-queue write, the stub write, and the orphan-feedback cleanup. Promote `--drop` AFTER demote (already landed) so the foreign-commit refusal pattern is stable.
- **`clank-wfw` / `unfinish-rewrites-history` (FINISHED)** — established the "rewrite engine + preview + transactional ordering" pattern that purge, demote, and now `--drop` all share.
- **`agent-add-cli-and-repo-scope` (active)** — independent of `--drop`; no overlap. `--drop` can land in any order relative to it.

## Open questions

These are decisions worth a real conversation at promote-time:

1. **Should `--drop` honor demote's `Rewrite`-requires-`--force` tier, or is the `--drop` flag itself the opt-in?**
   - Demote refuses `Rewrite` non-foreign without `--force` on the grounds that the user could lose their own code unintentionally. The flag pair is `demote` (safe by default) + `demote --force` (opt-in to losing code).
   - Under `--drop`, the user has ALREADY typed the destructive flag. A second `--force-drop-code` knob feels like ceremony, not safety.
   - **Tentative pick**: skip the `Rewrite` tier under `--drop` — the flag itself is the opt-in. Foreign refusal stays unconditional. Pin at promote-time.

2. **Foreign-commit refusal: unconditional, or does the `--into-branch` preview path soften it?**
   - Under `clank demote --into-branch`, foreign refusal still fires (demote's `safety_check` runs before the engine, regardless of `--into-branch`).
   - One could argue: `--into-branch` is preview-only, so showing the operator what a foreign-commit-inclusive drop would look like is informational, not destructive.
   - **Tentative pick**: match demote — refuse unconditionally even under `--into-branch`. Operator can run `--dry` to see the listing without touching refs. Pin at promote-time.

3. **Should `--drop` clean orphan feedback files (mirroring demote), or leave them?**
   - Demote does (`demote.rs:301-317`). Today's `clank purge` does NOT.
   - Cleaning is consistent with "the plan never happened." Not cleaning is consistent with "purge doesn't touch `.clank/agents/`."
   - **Tentative pick**: NOT clean, print a hint at the end. The smaller-surface choice. Revisit if it leads to feedback rot.

4. **Naming**: `--drop` vs `--with-implementation` vs `--all-commits` vs `--include-code`.
   - Lloyd's explicit wording was `--drop`. Default to that.
   - `--with-implementation` is more descriptive but verbose.
   - `--include-code` reads as additive to the existing purge ("include code in what we strip") which is the right mental model.
   - **Tentative pick**: `--drop`. Pin at promote-time; codex has caught naming inconsistencies on adjacent plans before.

5. **`--drop` on a finished plan**: allow or refuse-with-suggest-`unfinish`?
   - Mechanically works via `build_rewrite_preview`'s finished-plan path (`preview.rs:122-133`).
   - Semantically the finalize commit is "the plan landed," and dropping it scrubs that audit event.
   - **Tentative pick**: allow. `--drop` is already the destructive lane; no need for a second gate. Document the consequence in `--help`. Pin at promote-time.

6. **Squash + drop diagnostic wording**: today's `clank purge --squash` accepts a message; `--drop --squash` is being refused at parse time. Make sure the error names BOTH flags so the operator understands which one to drop (no pun intended).

7. **All-plans `--drop`**: out of scope for v1, but worth being explicit in the `--help` text that combining the two is refused, and pointing to the per-plan invocation as the workaround.
