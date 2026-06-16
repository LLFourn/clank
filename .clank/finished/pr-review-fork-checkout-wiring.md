# pr-review-fork-checkout-wiring

Wire `clank fork` and `clank pr-review` together so starting a PR
review can also put the code in front of you. Today they're fully
decoupled — `pr-review start` does no checkout, `fork --pr` makes a
worktree but doesn't start a review — so a user must know to run two
commands in the right order (the gap behind "pr-review start doesn't
create a worktree"). Add three composed entry points sharing ONE
implementation.

## Entry points

1. `clank fork --pr <n> --review` — existing `fork --pr` (linked
   worktree on the PR head, team seeded) PLUS scaffold the PR review
   in that new worktree.
2. `clank pr-review start <n> --fork` — identical to (1). Delegates
   to the same core; it's the same operation reached from the
   pr-review verb.
3. `clank pr-review start <n> --checkout` — no worktree: fetch the
   PR head and `git checkout` it IN THE CURRENT worktree, then
   scaffold the review here.

`--fork` and `--checkout` are mutually exclusive; neither = today's
behavior (scaffold in place, no git mutation).

## Architecture: one core, two callers (the whole point)

The risk is two divergent implementations of "make a worktree, then
start a review" behind `fork --review` and `pr-review --fork`. Avoid
it: a single core function — e.g. `fork_pr_with_review(source, pr,
…) -> worktree_path` — that does `run_fork` then `start_with` in the
returned dest. Both `fork.rs`'s `--review` branch and
`pr_review.rs`'s `--fork` branch call it. There must be no second
copy of the sequencing.

- `fork --pr --review`: after `run_fork` returns the dest, call
  `start_with(&dest, &slug, pr, Some(&source))` so the review
  scaffold lands in the worktree's gitignored `.clank/` (where the
  forked team's wait surface reads it). The slug comes from the
  source repo's origin (`repo_slug`), NOT the worktree cwd.
- `pr-review start --fork`: same core; the verb is just a second
  doorway. Its stdout should match `fork`'s contract (the worktree
  path is fork's SOLE stdout line — preserve it).
- `--review` requires `--pr` on fork (a fork off a branch has no PR
  to review) — clap-level conflict/requirement, erroring clearly.

## `--checkout` (in-place)

- Precondition FIRST (mirror fork's ordering): refuse if the
  worktree is dirty — a checkout would clobber uncommitted work.
  Reuse the existing dirty probe (`status::dirty_stats`); bail with
  a clear message before any fetch.
- Fetch the PR head (`fork::fetch_pr_head`) and check it out. Decide
  detached-HEAD on the sha vs a local branch `pr-<n>` — prefer a
  branch so the user can commit fixups; name it deterministically
  and refuse/relabel if it exists. Document the choice in the verb
  help.
- Then `start_with(repo, &slug, pr, None)` in place.
- This is the one path that mutates the user's current branch, so
  the help text must say so plainly.

## CLI surface

- `PrReviewStartArgs`: add `--fork` and `--checkout` bools (clap
  `conflicts_with`). Keep `<pr>` positional.
- `ForkArgs`: add `--review` bool, `requires = "pr"`.
- `main.rs`: dispatch unchanged (both flow through `run`).

## Testing

In-process, no clank-binary spawning (git spawns fine — these are
worktree/checkout ops):
- Core `fork_pr_with_review`: against a `setup_repo_with_pr`-style
  fixture, asserts the worktree exists AND
  `.clank/pr-reviews/<n>/pr.json` exists in the DEST with the
  source's slug + pinned head. One test, exercised by both callers.
- `pr-review start --fork` and `fork --pr --review` reach the same
  state (assert on the resulting files, not the code path).
- `--checkout`: on a clean fixture, HEAD moves to the PR head and
  the review scaffolds in place; on a DIRTY worktree it refuses
  before fetching (assert the error, assert HEAD unchanged).
- `--fork`/`--checkout` mutual exclusion is a clap parse error;
  `--review` without `--pr` is a clap error.

## Non-goals

- No auto-launch of the zellij session here (`fork` already stages
  sessions; `clank open zellij` / existing launch path is separate).
- No change to the review loop itself — this is purely the
  "obtain the code + scaffold" composition layer on top of the
  existing `start_with`.
- Depends on `pr-review-master-draft-phase` (the propose/round-0
  model) landing first; the scaffold these create is round 0 =
  master drafting, exactly as `start` produces today.
