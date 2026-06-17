# rewrite-scratch-dir-worktree

`clank purge --squash` / `clank finish --purge --squash` fail with
`Not a directory (os error 20)` in ANY linked worktree. The finalize
half succeeds (finish commit created, plan moved `plans/ →
finished/`); the purge+squash rewrite then aborts. Branch is left
unchanged — no corruption, but you're stuck with an un-squashed chain
plus a dangling finalize commit.

Nesting is a red herring (first seen in a nested worktree, but that's
not the trigger).

## Root cause (confirmed in code)

`crates/cli/src/cli/rewrite.rs` builds its scratch git index under a
hardcoded `<repo>/.git/`, in BOTH `build_stripped_tree` (purge) and
`build_stripped_tree_from_sha` (squash):

```rust
let scratch_dir = repo.join(".git").join("clank-rewrite");
std::fs::create_dir_all(&scratch_dir)?;   // <-- ENOTDIR here
```

In a linked worktree `<repo>/.git` is a FILE
(`gitdir: <main>/.git/worktrees/<id>`), not a directory, so
`create_dir_all("<worktree>/.git/clank-rewrite")` errors ENOTDIR.
Affects every linked worktree regardless of nesting depth; a
top-level checkout (where `.git` is a real dir) is unaffected — which
is why it slipped through.

## Fix

Resolve the real git dir instead of assuming `<repo>/.git`. Git gives
the right answer for both main checkouts and worktrees:

```rust
// absolute scratch path inside the actual gitdir
let scratch_dir = PathBuf::from(
    git_capture(repo, &["rev-parse", "--git-path", "clank-rewrite"])?.trim(),
);
```

`git rev-parse --git-path clank-rewrite` →
`<main>/.git/worktrees/<id>/clank-rewrite` in a worktree, and
`<repo>/.git/clank-rewrite` in a main checkout. (Equivalently
`--git-dir` / `--git-common-dir` then join.) Apply in BOTH functions.
The private-index commands already run via `-C repo` + `GIT_INDEX_FILE`,
so only the scratch-dir location needs to change.

## Reproduction

1. Make ANY linked worktree (`clank fork ...` or `git worktree add`)
   — nesting irrelevant.
2. In it: create + commit a plan, make a code commit, then
   `clank finish <p> --purge --squash "msg"`.
3. After clearing the dirty-tree precheck (below), the rewrite
   ENOTDIRs at `create_dir_all(<worktree>/.git/clank-rewrite)`.
4. A main-repo checkout does NOT reproduce (`.git` is a real dir).

Narrowing test for a fix: same flow in a single-level worktree should
succeed once the scratch dir is resolved via `--git-path`.

## Secondary, independent: dirty-tree precheck

Before the rewrite even runs, `finish`/`purge` reject the tree with
`working tree dirty; commit or stash first` because the precheck
counts UNTRACKED `.clank/` scratch (`cache/`, `agents/`,
`config.json`, `.gitignore`) as dirty — clank's own local files,
which shouldn't block a rewrite. Workaround used: append those paths
to `$(git rev-parse --git-path info/exclude)`. Consider ignoring
`.clank/`'s own scratch in the dirty check.

## Workaround (manual squash, engine-free)

`git reset --soft <plan-parent-sha>` → unstage the plan artifact
(`git restore --staged .clank/finished/<p>.md`) → one `git commit`.
Produces the identical purged + squashed result.
