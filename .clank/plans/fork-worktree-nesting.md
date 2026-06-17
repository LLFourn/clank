# fork-worktree-nesting

`clank fork` run from inside a worktree NESTS the new worktree under
the current one instead of placing it as a sibling. After a fork of a
fork you get paths like:

```
/Users/llfourn/src/frostsnap/.clank/worktrees/pr-497/.clank/worktrees/pr-498
```

— and it compounds with each level. Worktrees should always live flat
under the MAIN repo: `<main>/.clank/worktrees/<name>`.

## Cause

`run_fork` computes the default dest relative to `source` (the
CURRENT repo), and `source = resolve_repo(...)` is the current
worktree when fork is invoked from inside one (fork.rs:191):

```rust
None => source.join(format!(".clank/worktrees/{name}")),
```

So forking from worktree B drops C under `B/.clank/worktrees/`.

## Fix

Default the worktree dest under the MAIN worktree root, not the
current one. Resolve the main root from the shared git dir — e.g.
`git -C <source> rev-parse --path-format=absolute --git-common-dir`
gives `<main>/.git` for both a linked worktree and the main checkout,
so the main root is its parent (handle the `.git`-as-file case via
`--git-common-dir`, not `--show-toplevel`). Then:

```
dest = <main_root>/.clank/worktrees/<name>
```

- Forking from the main repo: `<main_root>` == source, so behavior is
  unchanged.
- Forking from a worktree: C lands as a SIBLING of B under
  `<main>/.clank/worktrees/`, never nested.
- `--path <dir>` explicit override is unchanged (caller's choice
  wins, as today).

## What stays anchored to the current worktree (do NOT change)

The fork SOURCE — the bound sessions to fork, and the base ref
(`HEAD`/`--branch`/pinned PR head) — must still come from the current
worktree B (you're forking B's work). Only the dest LOCATION moves to
the main root. So keep `source` for reading sessions + the worktree
base; compute `main_root` only for placing the dest.

## Knock-on details

- `ensure_clank_gitignore_entry(.., "/worktrees/")` should target the
  MAIN repo's `.clank/.gitignore` (where the worktrees dir actually
  lives), not the current worktree's.
- The `dest.exists()` collision check now checks under main (a fork
  name already used surfaces correctly regardless of which worktree
  you fork from).
- `git worktree add` is already run via `-C source`; it adds to the
  shared repo, so a main-rooted dest works whichever worktree invokes
  it.

## Testing

In-process (fork uses the core directly; git spawns fine):
- Pure: a `main_root` resolver given a repo path returns the main
  checkout for both a main repo and a linked worktree.
- Integration: create a worktree B, then `run_fork` FROM B (no
  `--path`) → the new worktree is `<main>/.clank/worktrees/<name>`
  (a sibling of B), NOT nested under B. Assert the path has exactly
  one `.clank/worktrees/` segment.
- Forking from main still lands under main (regression).

## Non-goals

- Changing the fork SOURCE/base semantics (still the current
  worktree's sessions + HEAD).
- Renaming/migrating already-nested worktrees created before the fix
  (one-off manual cleanup).
