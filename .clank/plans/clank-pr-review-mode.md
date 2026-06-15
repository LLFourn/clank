# clank-pr-review-mode

Run the clank multi-agent review loop against a GitHub PR instead
of against local plan commits. Master authors a set of inline
review comments; reviewers iterate on them through the normal
gate until the team agrees; the agreed set is posted to the PR as
individual review comments for the user to watch.

## The model

Reuse the existing shape — master proposes an artifact, reviewers
converge via the gate, a terminal step publishes — but the
artifact is a set of PR comments, not a code diff. Almost every
piece already exists; this plan is mostly wiring.

## Cycles are commits on a hidden ref

Each review cycle is a commit on `refs/clank/pr-review/<pr>`
(the same hidden-ref technique shelve/rewrite already use:
commit-tree + update-ref, never touching HEAD or any branch).
`clank pr-review propose` commits the current comments doc onto
that ref.

What this buys, all by construction:

- **Freeze is structural, not disciplinary.** A cycle IS
  content-addressed; editing-in-place is unrepresentable, so the
  stale-approval race (reviewer approves cycle 3, master edits
  cycle 3 underneath them) cannot occur.
- **`clank feedback write --commit <cycle-sha>` works VERBATIM.**
  Cycle feedback lands in
  `.clank/agents/<label>/feedback/<sha>.md` exactly like plan
  review — same verb, same skill docs, same sha-keyed
  `FsPlanStateLookup::reviews_for`, same short-sha resolution. No
  `--pr --cycle` flag variants, no second feedback path, no new
  reviewer teaching.
- **wfw wakes for free.** The watcher already covers the git dir
  — a ref update on propose wakes reviewers; a feedback write
  under `.clank/` wakes master. Zero watcher changes.
- **Cycles diff.** `git diff <cycle1>..<cycle2>` shows master's
  revision; reviewers review the DELTA, not the whole doc again.
- **GC-safe audit trail.** The ref protects the cycles and
  outlives the worktree — keep it after publish as the durable
  record of what was agreed (publish records the posted comment
  ids alongside).

`compute_gate` is reused UNCHANGED: feed it the latest cycle
sha's reviewer verdicts. The gate's notion of "latest reviewable"
maps onto "latest cycle".

## Layout

All review state lives with the forked team in the PR worktree
(see "Worktree & session model"); the hidden ref lives in the
shared git dir, visible from every worktree.

```
<pr-worktree>/.clank/pr-reviews/<pr>/      (gitignored)
  target.json     { repo, pr, head_sha (pinned), current_cycle,
                    published: [ {path, line, comment_id} ] }
  comments.md     the working draft master edits between cycles

refs/clank/pr-review/<pr>                  (shared git dir)
  cycle commits, each freezing comments.md at propose time
```

`target.json` is the "what PR are we reviewing" marker the
feature needs — gitignored under `.clank/pr-reviews/`, so it is
structurally uncommittable onto the PR branch (only
`.clank/plans/` and `.clank/finished/` are un-ignored).

## comments.md format

One heading per inline comment; a deterministic parser turns each
into a `gh api` call:

```
## crates/core/src/wait.rs:270
<comment body>

## crates/cli/src/main.rs:40 (side: LEFT)
<comment body>
```

Each `## path:line[-end] [(side: …)]` block → one comment.

## Wait-surface integration (the load-bearing piece)

The autonomous loop is the whole point, and the precedent is
blocks: non-commit FS state that joins the wait surface through a
side projection merged in `derive_status` (`blocks_for` on
`PlanStateLookup`). Same shape:

- A projection scans `.clank/pr-reviews/*/target.json` + the ref
  tip + sha-keyed feedback and contributes
  `WaitItem::PrReviewer { pr, cycle_sha }` /
  `WaitItem::PrMaster { pr, next }` into `WorkStatus`.
- `work_for` routes them per role exactly like plan items; the
  stop-hook hint is one line
  (`pr-review: review #123 @ <sha12>`).

## Tier semantics — milestone gating, no new policy

Map cycles onto the existing milestone rule with
`latest_touched_plan = false`: commit reviewers (codex) review
EVERY cycle; the gate tier (ruthless) wakes only when the commit
tier marks FINISHED — which for a PR review means "this comment
set is ready to publish". That is exactly the finish-milestone
semantics `gate-reviewers-only-plan-change-and-finish` shipped;
the PR analogue costs zero new gate code.

## Verbs

```
clank pr-review start <pr>   # fetch pull/<pr>/head, pin head_sha,
                             #   write target.json. Decoupled from
                             #   fork: works in ANY checkout of the
                             #   PR (reuses fork --pr's fetch+pin
                             #   helper). fork --pr is just the
                             #   convenient way to be standing in
                             #   a forked-team worktree first.
clank pr-review propose      # freeze comments.md as the next cycle
                             #   (commit onto refs/clank/pr-review/<pr>);
                             #   validate anchors against the pinned
                             #   head_sha BEFORE committing.
clank pr-review publish      # gate must be FINISHED; post each
                             #   comment via gh api, record ids,
                             #   resumable. Tears down NOTHING.
clank pr-review abort        # drop target.json + the ref. No
                             #   worktree teardown.
```

`finish` stays finalize-a-plan-into-history; `publish` is
emit-to-an-external-service — different terminal ops, distinct
verb. Publish is outward-facing: refuses unless the gate is
FINISHED; `--dry` prints the would-be `gh api` calls.

## Publish robustness

- **Validate anchors at PROPOSE time, not publish time.** Every
  `## path:line` heading must anchor to a line inside the pinned
  head_sha's diff (GitHub rejects comments on lines outside the
  PR diff). Failing at cycle-freeze gives master a fixable error
  in-loop; failing at publish would waste the whole converged
  review.
- **Idempotent, resumable publish.** Validate ALL comments
  first, then post one-by-one recording each posted comment id in
  `target.json`; a mid-publish failure (rate limit, network)
  resumes without double-posting (skip headings whose id is
  already recorded).
- Post against the pinned `head_sha` as `commit_id` so line
  anchors don't drift:

```
gh api repos/<repo>/pulls/<pr>/comments \
  -f body="..." -f commit_id=<head_sha> \
  -f path=<path> -F line=<n> -f side=RIGHT
```

## Worktree & session model (reconciled with `clank fork --pr`)

The stub's original "state in the main repo, PR worktree is a
read-only code area" model is REPLACED. `clank fork --pr` ships
the opposite and better shape: a whole forked team lives in the
PR worktree with their own bindings.

- **All review state lives in the worktree** — `target.json` and
  the working `comments.md` under the worktree's gitignored
  `.clank/pr-reviews/`, cycle feedback under the worktree's
  `.clank/agents/<label>/feedback/`, sessions bound there. From
  the team's perspective it is simply "the repo"; every existing
  command works unmodified.
- **The hidden ref lives in the shared git dir.** Refs are common
  across linked worktrees, so cycles are visible repo-wide and
  survive `git worktree remove` — the audit trail outlives the
  scratch.
- A checked-out PR branch carries its OWN committed
  `.clank/plans/` — never read it as clank state; the
  `pr-reviews/` scratch is what this feature reads.

## Relationship to `clank fork --pr` (decision 1)

`fork --pr` does NOT auto-run `pr-review start`. Fork stays the
generic "team + worktree on this PR" verb; review mode is opt-in.
The fork orientation prompt MENTIONS `clank pr-review start <pr>`
so master kicks it off as its first act. Keeps the two commands
composable and review-mode explicit.

## Teardown (decision 2)

`publish` tears down NOTHING. It posts, records comment ids in
`target.json`, prints a `git worktree remove <path>` hint, and
exits. Consistent with fork's "no `--rm` wrapper, use plain git"
philosophy, and the team may keep working (e.g. pushing genuine
fix-commits to the PR branch) after publishing. `abort` likewise
only drops the review scratch + ref, never the worktree.

## PR advances mid-review (decision 3)

Pin + warn for v1; NO automatic re-pinning. `propose` checks
whether `pull/<pr>/head` still matches the pinned `head_sha`; on
mismatch it WARNS (`PR advanced past the pinned sha — comments
target the old snapshot; rerun clank pr-review start --repin to
review the new head, which opens a fresh cycle`) and proceeds
against the pin. Master decides whether to re-pin. Re-pin =
new fetch + new pin + next cycle against the new code — reusing
existing machinery, no line-migration engine.

## Status surfacing

`clank status` + the TUI gain a `pr` gauge (shelf precedent):
`pr #123  cycle 3 @ <sha12>  waiting on codex`. The bar carries
it when it's the only active work.

## Status

Design converged with lloyd across 2026-06-09/10 and reconciled
2026-06-15 against the shipped `clank fork --pr` substrate. All
open questions resolved: cycles-as-commits on
`refs/clank/pr-review/<pr>` (DECIDED); state lives in the fork
worktree with the ref in the shared git dir; fork does not
auto-start review (decision 1); publish/abort tear down nothing
(decision 2); pin + warn, no auto-repin (decision 3). Ready to
queue and implement.
