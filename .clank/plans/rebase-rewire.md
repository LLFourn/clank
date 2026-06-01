# rebase-rewire

When a rebase rewrites commit SHAs, feedback files at
`.clank/agents/<author>/feedback/<old-sha>.md` become orphaned —
the new commits have no attached feedback. Wire a git
`post-rewrite` hook into `clank init` that copies the feedback
forward so each new SHA inherits the latest feedback from the
old chain.

## Why copy, not move

The user's framing: "if they reset --hard or something it would
be good if the feedback came back." Copy semantics give us that
for free — the old `<old>.md` stays on disk, so a `reset --hard
<old>` brings the feedback back automatically. Drift cost is
small (a few KB per rebased commit). Move semantics would force
us to track "where did this used to be?" to restore, which
isn't worth the complexity.

Feedback isn't precious — the user noted you can always ask the
agent to re-review. Copy keeps the option open without
machinery.

## Install (via `clank init`)

`clank init` already scaffolds `.clank/`. Extend it to also
write `.git/hooks/post-rewrite` (mode 0755) containing:

```sh
#!/usr/bin/env sh
# clank rewire hook
exec clank rewire --from-stdin
```

Git invokes `post-rewrite` with a positional arg (`amend` or
`rebase`); we deliberately drop it because `clank rewire`
treats both the same way. If we add per-mode behavior later,
the CLI grows an optional positional then.

Behavior:
- If `.git/hooks/post-rewrite` doesn't exist → write it.
- If it exists and is already the clank version (header match
  on a `# clank rewire hook` comment we include in the script)
  → leave alone.
- If it exists and is a foreign hook → print a warning telling
  the user to chain it manually, don't clobber. Same pattern
  as `clank setup` with stop-hook installation.
- `clank init --force-hooks` overwrites foreign hooks.

`clank init` is also where re-runs are safe today (idempotent
scaffold). Adding the hook install fits that contract.

## New command: `clank rewire --from-stdin`

Reads the `post-rewrite` stdin format — one line per pair,
`<old-sha> <new-sha> [extra]`. For interactive rebases with
squash/fixup, multiple old SHAs map to the same new SHA; the
stream reports them in rebase processing order (chronological
in the original branch).

Algorithm:

1. Read every line; collect `Vec<(old, new)>` in input order.
2. Group by `new`; within each group keep the LAST `old` in
   input order (= most recent commit in the squash range).
   Discard earlier `old` shas in the group.
3. For each `(latest_old, new)` pair, for every author dir under
   `.clank/agents/*/`, look for the source file in the same
   order `FsReviewLookup::reviews_for` does:
   - `feedback/<latest_old_full>.md` (40-char) first;
   - then `feedback/<latest_old_short>.md` (first 7 chars of
     `latest_old`).
   - First hit wins. If neither exists → do nothing. This
     implements the user's "if the most recent commit in the
     squash range has no feedback, drop all" rule by simply
     not copying anything.
4. Destination is always written as
   `feedback/<new_full>.md`. The reader checks full first,
   so writing full guarantees the new SHA's feedback is
   found regardless of whether the source was short- or
   full-form. Overwrite if it already exists — the new SHA is
   authoritative.

No `git add`. Feedback files are gitignored by design (see
root `.gitignore`'s `.clank/*` + `.clank/agents/` carve-outs);
they live as per-clone local state. The hook copies in place;
git's index never sees the file.

The OLD feedback files are left untouched. Reset --hard back to
an old SHA continues to work.

## Edge cases

- **Amend**: one pair, straightforward copy. Old feedback file
  on the pre-amend SHA still exists; the amended commit gets
  its own copy.
- **Reorder-only rebase**: one-to-one pairs, each handled
  independently.
- **Commit dropped from rebase (e.g. interactive `d`)**: no
  pair is emitted for the dropped commit. Its feedback stays
  at the old SHA. Nothing to do — by design.
- **Same-commit chain that touches multiple agents**: each
  author dir is processed independently. If alice has feedback
  on the old SHA but bob doesn't, only alice's file gets
  copied forward.
- **No `.clank/agents/` directory**: hook is a no-op (the
  scanner skips silently).
- **Author dir contains non-`<sha>.md` files**: ignored (the
  scanner only probes the two exact filenames it expects —
  `<full-sha>.md` and `<short-sha>.md` — and skips anything
  else).
- **Running outside a clank repo**: `clank rewire` resolves
  the repo root via the same `resolve_repo` everything else
  uses. If `.clank/` is absent, no-op.

## Tests

Unit, on a synthetic `.clank/agents/` tree:

- `rewire_copies_feedback_on_simple_rename` — one (old, new),
  feedback file present → new file appears, old file untouched.
- `rewire_skips_when_old_has_no_feedback` — pair given but
  source file missing → no-op, no errors.
- `rewire_squash_keeps_only_latest_old` — three pairs all
  mapping to one new sha; only the LAST old's feedback (if
  present) is copied; the others are dropped.
- `rewire_squash_drops_all_when_latest_has_no_feedback` —
  same shape but the last old has no file → no copy, even if
  earlier olds did.
- `rewire_handles_multiple_authors` — alice + bob feedback
  on the same old sha → both copy forward.
- `rewire_source_short_form_writes_destination_full` — source
  file is `<short>.md`, destination must land at
  `<full-new>.md` (matching the reader's lookup order).
- `rewire_destination_always_full_even_when_source_full` —
  same outcome regardless of source filename mode; pins the
  "always write full" invariant.
- `rewire_does_not_touch_index` — after rewire on a repo with
  ignored `.clank/agents/`, `git status --porcelain` is
  unchanged (no entries appear in the index).

Integration (spawns clank as the hook):

- `post_rewrite_hook_installed_on_clank_init` — fresh repo,
  run `clank init`, assert `.git/hooks/post-rewrite` exists +
  is executable + invokes `clank rewire --from-stdin`.
- `clank_init_does_not_clobber_foreign_hook` — pre-existing
  post-rewrite hook → `clank init` warns and leaves it alone;
  `--force-hooks` overwrites.
- `actual_rebase_rewires_feedback_end_to_end` — real `git
  rebase -i` with a squash; after the rebase, on-disk
  `.clank/agents/<author>/feedback/<new-sha>.md` exists with
  the expected content, and `git status --porcelain` is
  unchanged (the files are gitignored — never staged).

## Out of scope

- Cleanup of orphaned feedback at old SHAs. They're harmless
  and load-bearing for reset-restore. A `clank prune-feedback`
  command can come later if the cruft becomes a real concern.
- Cherry-pick and merge handling. `post-rewrite` doesn't fire
  on cherry-pick (it fires on amend/rebase only); cherry-pick
  is conceptually a fresh commit anyway.
- Tracking "feedback provenance" (which historical SHA this
  feedback was originally on). Not needed for the copy model.
- `core.hooksPath` indirection. Per-clone install via
  `.git/hooks/` is the standard pattern; users with custom
  hookspath already know they need to wire clank in.
- Touching the git index at all. Feedback files are
  gitignored by design; the hook copies on disk and never
  runs `git add` / `git commit`. Both staging and
  auto-committing are out of scope.
