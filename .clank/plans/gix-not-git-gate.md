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

- ONE module is the sole git authority (extend `git_io.rs` — already the gix
  read layer — broadening its contract from "reads only" to "all git access,
  read+write, gix-or-subprocess"; rename if clearer).
- Every caller uses a TYPED function there (`commit_subject`, `worktree_add`,
  `status_porcelain`, …). No caller constructs `Command::new("git")` or calls
  `gix::*` directly — including the gix calls the recent dirty-stats work
  scattered into `status.rs` / `worktree_facts.rs` / `fs_plan_state_lookup.rs`,
  which fold back in.
- Inside the layer, each function picks the backend. gix where viable;
  subprocess only where gix genuinely can't (yet). The boundary makes a later
  git→gix swap INVISIBLE to callers — that's the payoff.

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
  usage outside the one layer module.
- EXCLUDES test code (`#[cfg(test)]` modules and `crates/*/tests/`) — fixtures
  legitimately spawn `git` and open gix repos. Suggested mechanism:
  brace-track from each `#[cfg(test)]` attribute; skip integration dirs.
- Because everything is centralized, the test is a simple single-file
  boundary check — no allowlist, no per-file counts.
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

- One module is the sole site of `Command::new("git")` and `gix::` use;
  callers use typed functions and are backend-agnostic.
- The listed easy ops are gix; the hard ops are subprocess inside the layer.
- A raw `git`/`gix` use outside the layer (in production code) FAILS
  `cargo test`; test fixtures are unaffected.
- The rule is documented for agents.
