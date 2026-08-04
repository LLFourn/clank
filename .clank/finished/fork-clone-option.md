# fork-clone-option
# `clank fork --clone`: a full clone instead of a linked worktree

## Why

A linked worktree shares the parent repo's git config — including
remotes. There is no way to give a fork a different remote set: an
agent working in a worktree can always see (and push to) the same
github remotes as the main checkout. The user wants forks whose
remotes can be REMOVED — an independent repository.

A LOCAL clone gives exactly that: `git clone <source> <dest>` makes
an independent repo whose only remote is `origin = <source path>`.
Network remotes (github) are absent by construction; the fork can
still fetch from / push back to the parent repo through the local
origin, and the user (or a later plan) can `git remote remove
origin` inside the clone without touching the parent.

## What

- `clank fork <name> --clone` clones the main repo root instead of
  `git worktree add`. Everything else about fork stays: the pinned
  base sha, the branch named `<name>` created at that base, session
  seeding into the dest's gitignored `.clank/`, the review scaffold,
  the SOLE-stdout-line = dest path contract, and the auto-spawn
  doorway.
- Destination: `.clank/clones/<name>` under the main repo root —
  parallel to `.clank/worktrees/<name>`. `--clone` CONFLICTS with
  `--path` (clap `conflicts_with`): a relocated clone would defeat
  every discovery/lifecycle contract below, and kind is never
  inferred from a path anyway (see the descriptor).
- **Fork descriptor = the durable kind + identity record** (intro
  review f695e8b): creation writes `<dest>/.clank/fork.json` —
  `{ "kind": "clone", "source": "<canonical main-root path>",
  "branch": "<name>", "base": "<sha>" }` — gitignored via the
  inherited `.clank/.gitignore` allow-list. Every later decision
  (idempotent re-run, discovery, teardown) reads THIS record; none
  consults the remote set, because `git remote remove origin` is the
  motivating act and must not break any lifecycle operation.
- The clone is branch-pinned like the worktree arm: clone, then
  `checkout -b <name> <base-sha>` (fail-closed pre-flight before any
  directory is created, same as today).
- Idempotence: dest exists → require a descriptor matching (kind
  `clone`, source = this repo's canonical main root, branch
  `<name>`) AND a git repo actually on branch `<name>`; then no-op
  returning the path. Missing or mismatched descriptor → loud
  failure naming the dest (a foreign repo squatting the path is
  never adopted).
- **Logical vs transport source**: seeding state — pinned base sha,
  sessions, roster, drafts — comes from the CURRENT worktree exactly
  as the worktree arm does today; only the TRANSPORT is main-rooted:
  the clone's origin URL, the descriptor's `source`, and the dest
  path all use the canonical main repo root (the pinned sha is
  reachable there via the shared ref store). Forking from a linked
  worktree therefore seeds that worktree's state into a main-rooted
  clone.
- Remotes: keep the implicit local `origin` (flow-back is useful and
  removing it is now POSSIBLE, which is the point). No strip-remotes
  flag.
- **Doorways**: `clank open zellij --fork/--all` discovers forks via
  `git worktree list --porcelain` today, which structurally cannot
  see clones — discovery gains a second leg scanning
  `.clank/clones/*/` for descriptor-bearing repos. Name collisions
  are prevented at CREATION: `<name>` is one namespace across both
  kinds — `fork x --clone` fails if worktree `x` exists and vice
  versa — so discovery never has to resolve a same-name pair.

## How (constraints)

- The clone goes through `git_plumbing` (a new `clone_local(source,
  dest, branch, base)` beside `worktree_add`) with the usual one-line
  "why not gix" justification; `git_boundary.rs` stays green.
- Local clone default behavior (hardlinked objects) is fine — object
  sharing is safe; it is the CONFIG independence we're after. Note it
  in the function doc.
- Survey the cleanup/lifecycle paths that assume worktrees
  (`git worktree remove`, purge/shelve, fork-worktree-nesting
  sibling logic) and make them clone-aware where they'd break,
  keyed off the DESCRIPTOR's `kind`, never off path shape or remote
  state; a clone is removed with plain directory deletion, never
  `git worktree remove`.
- Forking FROM a clone: out of scope — document that `--clone` forks
  from the main repo root like today's worktree arm, and a clone is
  not a fork source.
- `--clone` also conflicts with `--pr` (v1): a fetched PR head is
  reachable from no main-root ref, and while same-filesystem local
  clones happen to copy the whole object store today, git does not
  guarantee unreachable objects survive a clone — the conflict is
  conservative scope; lift later with an explicit post-clone
  transfer if the combination is wanted.

## Acceptance

- `clank fork x --clone` produces `.clank/clones/x`: independent
  repo, branch `x` at the pinned base, `origin` = main repo root
  path, no other remotes, a `kind: clone` descriptor, sessions
  seeded, path on stdout. `--clone --path` is rejected by clap.
- `git remote remove origin` inside the clone succeeds, leaves the
  parent repo's remotes untouched, and a SUBSEQUENT `clank fork x
  --clone` re-run is still the no-op (idempotence survives the
  motivating act — covered by a test).
- Re-run is a no-op returning the same path; a dest that exists
  without a matching descriptor fails loudly; `fork x --clone` with
  an existing worktree `x` (or the reverse) fails at creation.
- Forking from inside a linked worktree seeds THAT worktree's
  sessions/base into the main-rooted clone (test).
- Worktree behavior (no `--clone`) is byte-for-byte unchanged;
  existing fork tests stay green.
- In-process tests only (library cores; fixtures may spawn git).
