# gix-fold-walker

Replace the fold's per-commit stateless IO helpers with one
stateful gix walker, so folding N commits costs N cheap tree
comparisons plus a full diff only for commits that actually touch
`.clank/`.

## Motivation

`clank status --tui` pointed at bdk burned 70% CPU continuously
(264 CPU-minutes in one evening). Profile: ~70% of samples inside
`rebuild_from` → `git_io::diff_tree_changes` → `gix::open`. Two
compounding costs, both in the event producer:

1. `diff_tree_changes(path, sha)` calls `gix::open` per commit —
   re-enumerating the entire object DB (`fstatat` storm over pack
   files) for every single commit folded.
2. It then does a full recursive diff of the whole repo tree just
   to learn (a) which `.clank/` files changed (usually none) and
   (b) whether anything else changed (one bit, `touched_code`).
   In a host repo like bdk that's thousands of full-tree diffs
   for commits that predate `.clank` entirely or only touch
   upstream code.

The sans-IO fold (`apply_commit` consuming `CommitEvent`s) is NOT
the problem and does not change. The producer was built as
independent stateless helpers (`first_parent_commits_*` to list
shas, `diff_tree_changes` per sha, each opening the repo) — a
decomposition that throws away everything a traversal naturally
carries: the open repo handle, and the parent tree you just had.

## Design

One stateful walker owning the traversal, in `git_io`:

- Open `gix::Repository` ONCE per fold; thread it through.
- Walk first-parent from base to tip carrying the previous
  commit's root tree along (it is the next commit's parent tree).
- Per commit, compare parent vs child root trees ONE level deep:
  - `.clank` entry OID unchanged (or absent on both sides) → no
    clank changes; skip the diff entirely. This subsumes "start
    the fold where `.clank` was first added": every pre-clank
    commit is absent==absent, near-free.
  - any OTHER top-level entry differs → `touched_code = true`,
    no recursion needed.
  - `.clank` entry OID changed → real diff. REFINED at impl time:
    the full-repo diff (today's exact computation), NOT a
    subtree-only diff — renames crossing the `.clank` boundary
    (plan moved out of / resurrected into `.clank/plans/`) lose
    their rename pairing in a subtree diff, silently changing
    `PlanTouch` semantics. Clank-touching commits are the rare
    case, so the win is unaffected and equivalence with the
    legacy producer becomes exact rather than approximate.
- Emit the same `CommitEvent { sha, author_ts, subject, changes }`
  — `apply_commit` and everything in core is untouched.

This is the same trick `git log -- .clank` uses internally
(pathspec limiting via tree-entry OID comparison); we do it
inside our walk because the fold needs subjects + the
touched_code bit for every commit, not just clank-touching ones.

Callers to migrate: the fold loops in `rebuild_with_diagnostics`
/ `fold_forward` and both phases of `rebuild_from` (rebuild.rs).
The old per-commit helpers stay only if non-fold callers remain;
otherwise delete.

Sized at impl time: `git_io::snapshot` (the cold fold's producer,
feeding `derive_state`) is a fourth instance of the same pattern —
migrated too. `preview.rs` (purge previews) also walks with
per-commit diffs but additionally needs a per-commit merge bit
that `CommitEvent` doesn't carry; it is a one-shot command path,
not the hot fold, so it deliberately stays on the legacy helpers
(which therefore remain as production API and double as the
equivalence test's reference implementation).

## Correctness notes

- Rename detection: current `diff_tree_changes` uses gix rename
  detection at 50% similarity. Preserve those semantics for the
  `.clank` subtree diff; add an equivalence test old-vs-new
  producer over a synthetic repo exercising adds/renames/deletes,
  root commit, merge (first-parent), and pre-clank history.
- Root commit: parent tree = empty tree (matches legacy `--root`).
- `touched_code` semantics must match today's: any non-`.clank`
  path changed. One-level root comparison gives exactly that.

## Testing

In-process only (no binary spawning). Equivalence test: fold a
synthetic repo with both producers, assert identical CommitEvent
streams and identical final RepoState. Keep unit tests for the
one-level comparison logic as a pure function over (parent
entries, child entries) if it factors cleanly.

## Non-goals

- Caching strategy changes (separate plan: fold-checkpoint-cache,
  queued after this — the walker makes per-commit depth tracking
  free, which that plan needs).
- TUI statefulness (in-memory fold session) — future work; this
  plan makes rebuilds cheap, not less frequent.
