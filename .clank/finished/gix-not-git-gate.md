# gix-not-git-gate

**Centralize ALL git access (gix AND subprocess) behind one module, then a
test forbids raw `git`/`gix` outside it.** Despite several plans to purge
`git` shell-outs for gix, agents keep reintroducing
`std::process::Command::new("git")` (most recently `dirty_stats`,
`worktree_status` in the status-tui CPU investigation) because nothing stops
it AND there's no obvious home for git logic — so each command module grows
its own ad-hoc `run_git`. The fix isn't a one-site patch or a scattered
allowlist; it's a clean BOUNDARY: one layer owns the backend choice, callers
are unaware whether a call is gix or a subprocess.

## The model

Two sanctioned layer files already exist and are kept (lloyd) — the
reads/writes split is intentional (`git_io`'s doc keeps reads read-only for
the filesystem-truth model):
- `git_io.rs` — ALL git READS (gix).
- `git_plumbing.rs` — ALL git MUTATIONS. Today gix object/tree/ref writes;
  this plan also moves the subprocess-only mutations here (worktree add/list,
  fetch, checkout, commit/amend/rm/add, cherry-pick) since gix can't do them
  yet — gix-or-subprocess, the caller can't tell.
- Every caller uses a TYPED function in one of those two. No caller constructs
  `Command::new("git")` or calls `gix::*` directly — including the gix calls
  the recent dirty-stats work scattered into `status.rs` / `worktree_facts.rs`
  / `fs_plan_state_lookup.rs`, which fold back into `git_io`.
- To keep the single-handle-per-snapshot optimization without leaking `gix::`
  to callers, `git_io` exposes an OPAQUE handle (e.g. `git_io::Repo` wrapping
  `gix::Repository`); `from_state` opens it once and passes it to the dirty
  walk and the per-plan worktree-status reads.
- The boundary makes a later git→gix swap (worktree, fetch, …) INVISIBLE to
  callers — that's the payoff.

## Scope of this plan

Centralize + convert-the-easy-ones + the boundary test. Convert in the SAME
pass where an equivalent already exists or is trivial; keep the hard ops as
subprocess *inside the layer* (convert later, behind the stable API).

**Convert to gix now** (equivalent exists in `git_io.rs` or trivial):
- `rev-parse HEAD` → existing `rev_parse_head` (purge, rewrite)
- `log -1 --format=%s` / `show -s --format=%s` → existing `commit_subject`
  (rewrite; finish tests)
- `diff-tree --name-status -r` → existing `diff_tree_changes` (unfinish)
- `status --porcelain` → the gix status path from `status-dirty-stats-via-gix`
  (purge, unfinish, shelve, open)
- `rev-list` (first-parent/reverse), `ls-tree -r --name-only`,
  `remote get-url origin`, `log -1 --format=<author/date/body>` (log.rs),
  `check-ignore` → straightforward gix (revwalk / tree traversal / config /
  commit metadata / gix-ignore, now available via the `status` feature).

**Keep as subprocess, but moved into the layer as typed fns** (gix can't, or
not safely, today):
- `worktree add` / `worktree list --porcelain` (fork, open_zellij)
- `fetch origin` (fork), `checkout -b` (pr_review)
- `commit` / `--amend` / `rm --cached` / `add` (purge, queue)
- `cherry-pick` (shelve), and the history-rewrite engine (rewrite, purge)

## The boundary test (no-binary-spawning — in-process scan)

- A `cargo test` (this repo has no separate gauntlet/CI) scans PRODUCTION
  source under `crates/*/src` and FAILS on `Command::new("git")` or `gix::`
  usage outside the two layer files (`git_io.rs`, `git_plumbing.rs`).
- EXCLUDES test code (`#[cfg(test)]` modules and `crates/*/tests/`) — fixtures
  legitimately spawn `git` and open gix repos. Suggested mechanism:
  brace-track from each `#[cfg(test)]` attribute; skip integration dirs.
- Because everything is centralized, the test is a simple two-file boundary
  check — no allowlist, no per-file counts.
- Tests: the scan flags a planted raw `git`/`gix` use in a non-test fixture
  and ignores one in a `#[cfg(test)]` module; the real tree passes.

## Documentation

- CLAUDE.md + the master skill: all git/gix access goes through the layer;
  adding a subprocess there needs a one-line justification (gix can't do X).

## Out of scope

- Converting the must-stay-subprocess ops to gix (worktree, fetch, checkout,
  history rewrite). They live behind the layer now; convert later without
  touching callers.

## Acceptance

- `git_io.rs` + `git_plumbing.rs` are the sole sites of `Command::new("git")`
  and `gix::` use; callers use typed functions and are backend-agnostic.
- The listed easy ops are gix; the hard ops are subprocess inside the layer.
- A raw `git`/`gix` use outside the layer (in production code) FAILS
  `cargo test`; test fixtures are unaffected.
- The rule is documented for agents.
