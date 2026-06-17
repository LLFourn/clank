# actually-replace-git-with-gix

The read-introspection layer (`git_io.rs`) was migrated off shell-out
to `gix` ([[replace-git-io-with-gix]], [[gix-fold-walker]]). But ~30
source files still shell out to the `git` binary. This plan extends
the migration across the REST — replacing every shell-out where gix is
a clean win, and KEEPING the CLI only where shelling out is
load-bearing, with a documented reason for each.

The end-state is explicitly NOT "zero shell-outs". The principle: the
`git` binary is the reference implementation wired to the user's
config, keys, hooks, and network — gix wins exactly when an operation
has none of those couplings (deterministic, config-independent), and
loses when it does.

## Inventory (src only, by subcommand frequency)

`commit` 29 · `rev-parse` 26 · `config` 21 · `add` 20 · `show` 11 ·
`init` 11 · `status` 8 · `log` 7 · `checkout` 6 · `update-ref` 4 ·
`symbolic-ref` 4 · `ls-tree` 4 · `worktree` 3 · `diff-tree` 3 ·
`branch` 3 · `write-tree` 2 · `update-index` 2 · `rev-list` 2 ·
`reset` 2 · `read-tree` 2 · `commit-tree` 2 · `remote` 1 ·
`merge-base` 1 · `fetch` 1. Heaviest sites: `rewrite.rs` (15),
`purge.rs` (13), `html.rs` (8).

## MIGRATE — clean wins (config-independent reads + pure plumbing)

No hooks, no network, no working-tree-config dependence; typed gix
results replace subprocess fork + text-parse fragility.

- Path/SHA queries: `rev-parse` (`--git-path`, `--git-common-dir`,
  `--show-toplevel`, `HEAD`, `<sha>^`), `symbolic-ref`, `merge-base`,
  `rev-list` → `Repository::{git_dir,common_dir,work_dir}`,
  `rev_parse`, `rev_walk`, `merge_base`. (NB: the worktree-root + git-
  path resolution that [[fork-worktree-nesting]] and
  [[rewrite-scratch-dir-worktree]] just fixed via `rev-parse` are
  prime migration targets — gix exposes them directly.)
- Object/tree reads: `show`/`cat-file`, `ls-tree`, `diff-tree` →
  already-proven gix idioms from `git_io.rs`.
- Config READS: `config --get …` (e.g. `branch.<n>.protect`,
  `remote.origin.url`) → `Repository::config_snapshot`.
- The `rewrite.rs` PLUMBING: `read-tree`/`write-tree`/`commit-tree`/
  `update-index`/`update-ref`/`hash-object` → gix object writes,
  in-memory tree/index editing, and ref transactions. This removes
  the `GIT_INDEX_FILE` scratch-dir dance entirely (the very thing that
  ENOTDIR'd in worktrees). High value, but correctness-critical —
  migrate behind the existing parity tests + the live-worktree tests.

## GATE — verify gix parity FIRST; keep CLI + document if not

Each of these is shelled for a real reason. The plan must DECIDE
(don't migrate blindly):

- `git commit` (×29) — runs the user's hooks (`pre-commit`,
  `commit-msg`, …) and GPG/SSH-signs per `commit.gpgsign`. A gix commit
  bypasses BOTH. Decision: keep `git commit` shelling unless we
  deliberately want to drop hook+signing fidelity (we don't). Likely
  KEEP. (Clank's own `commit-tree` plumbing in `rewrite.rs` is
  separate — that's already hook-less by design and IS migratable.)
- `git fetch` (×1, network) — needs the user's credential helpers,
  SSH agent, `insteadOf`, proxies, known-hosts. gix network +
  credential handling is a large surface the read-migration
  deliberately avoided. KEEP unless a strong case emerges.
- `git worktree add` (×3) — gix worktree CREATION (vs read) may be
  immature in 0.84. Spike it; KEEP if no clean equivalent. clank is
  worktree-heavy, so correctness > purity here.
- `git status --porcelain` (dirty checks) + `checkout`/`reset --hard`
  (worktree resync) — must match git's gitignore/`fileMode`/`autocrlf`/
  symlink/sparse semantics exactly (a mismatch reintroduces the
  dirty-check bug class). Verify gix worktree-status/checkout parity;
  migrate only with live parity tests, else KEEP.
- `git init` (×11) — mostly test scaffolding; src uses are few. Low
  priority; migrate if trivial.

## Approach

- Incremental, function-by-function, behind typed APIs — the model
  that made [[replace-git-io-with-gix]] safe (backend swap, identical
  signatures + tests). Add a write/plumbing counterpart to
  `git_io.rs` rather than scattering `gix::` calls.
- Open `Repository` per-call to start (matches the read migration);
  a cached handle is a later optimization, not a prerequisite.
- One PR-sized milestone per cluster (path/sha reads, config reads,
  object/tree reads, rewrite plumbing, then the gated decisions), each
  green before the next.

## Testing

- Per-function parity tests at the API surface (identical to the read
  migration's discipline).
- LIVE tempdir tests with REAL repos AND linked worktrees for anything
  touching the index, refs, or worktree state — the worktree `.git`-is-
  a-file shape must be exercised (the rewrite plumbing especially).
- The existing integration suites (`*_integration.rs`) stay green
  throughout — they already drive real git repos.

## Non-goals

- Zero shell-outs as a goal in itself. The target is "gix where it's a
  clean win; CLI where load-bearing, documented".
- Reimplementing hooks/signing or network/credentials in-process.
- sha256 repositories (gix `sha1` feature only, as today).
- A cached `Repository` handle (separate perf follow-up).
