# adhoc-review

## Summary

Two changes that simplify the review model:

1. **Feedback is per-commit, not per-plan.** Move from
   `.clank/agents/<author>/feedback/<plan>/<sha>.md` to
   `.clank/agents/<author>/feedback/<sha>.md`. No more
   `FeedbackTarget` enum, no `_` ad-hoc path, no plan-scoped
   scanning. One file per (author, commit).

2. **Ad-hoc commits are reviewable.** The existing
   `force_review_on_misc_commits` config (default true) is
   wired into work derivation so wfw surfaces unreviewed
   ad-hoc commits.

Both fall out of the same model change: reviews are on commits,
the fold knows which commits exist (plan + ad-hoc), so work
derivation covers everything.

## What already exists

- `config.rs`: `ReviewConfig.force_review_on_misc_commits`
  (default true), `ad_hoc_reviewers`, two-layer loading.
- `disk_format.rs`: `FeedbackTarget::Plan(PlanKey)` and
  `FeedbackTarget::AdHoc` with `_` segment.
- `RepoState.ad_hoc: Vec<AdHocEvent>`.
- `plan_view::evaluate` computes gate state from feedback.

## Migration

Move existing feedback files from
`.clank/agents/<author>/feedback/<plan>/<sha>.md` to
`.clank/agents/<author>/feedback/<sha>.md`.

`clank doctor` or a one-shot migration command scans the old
layout and moves files. If multiple plan directories have
feedback for the same (author, sha), keep one (arbitrary —
this shouldn't happen in practice). Delete empty plan
directories after migration.

The old `FeedbackTarget` enum and plan-scoped scanning are
removed. `disk_format.rs` feedback parsing simplifies to
just `<author>/feedback/<sha>.md`.

## Design: `derive_status` + `ReviewLookup` trait

### The trait

```rust
pub trait ReviewLookup {
    fn reviews_for(&self, sha: &CommitSha) -> Vec<ReviewEntry>;
    fn worktree_status(&self, plan: &PlanKey) -> PlanWorktreeStatus;
}

pub struct ReviewEntry {
    pub author: AgentLabel,
    pub verdict: Verdict,
}
```

No target parameter — feedback is commit-scoped.

### `RepoState::derive_status`

```rust
impl RepoState {
    pub fn derive_status(
        &self,
        reviews: &impl ReviewLookup,
        policy: &WorkPolicy,
    ) -> WorkStatus { ... }
}
```

### `WorkPolicy`

Named `WorkPolicy` to avoid collision with the existing
`ReviewPolicy` enum in `repo_state.rs`:

```rust
pub struct WorkPolicy {
    pub force_review_on_misc_commits: bool,
    pub ad_hoc_reviewers: Option<Vec<AgentLabel>>,
}
```

### `WorkStatus`

```rust
pub struct WorkStatus {
    pub plans: Vec<PlanWorkState>,
    pub ad_hoc: Vec<AdHocWorkState>,
}

pub struct PlanWorkState {
    pub plan: PlanKey,
    pub sha: CommitSha,
    pub gate: CommitGateState,
    pub waiting_on: WaitingOn,
}

pub struct AdHocWorkState {
    pub sha: CommitSha,
    pub gate: CommitGateState,
}

impl WorkStatus {
    pub fn work_for(
        &self,
        author: &AgentLabel,
        role: Role,
    ) -> Vec<WaitItem> { ... }
}
```

### Gate rule

Simple: any approve → approved, any request_changes →
changes_requested, else unreviewed. A single approve unblocks.
Cumulative participant tracking deferred.

### WaitItem

Add to `WaitItem`:

```rust
AdHocReview {
    sha: CommitSha,
    feedback_path: String,
},
AdHocRevise {
    sha: CommitSha,
},
```

### `clank feedback write`

Simplify: remove `--plan` entirely. Just `--commit <sha>` +
`--verdict` + `-m`. The tool writes to
`.clank/agents/<author>/feedback/<sha>.md`. No plan or adhoc
flag needed.

## Implementation surface

### `crates/core`

- New: `ReviewLookup` trait, `ReviewEntry`, `WorkPolicy`,
  `WorkStatus`, `PlanWorkState`, `AdHocWorkState`.
- New: `RepoState::derive_status` method.
- New: `WorkStatus::work_for` method.
- Add `AdHocReview` / `AdHocRevise` to `WaitItem`.

### `crates/cli`

- `FsReviewLookup` implements `ReviewLookup`: scans
  `.clank/agents/*/feedback/<sha>.md`.
- `wfw.rs`: load config → build `WorkPolicy` →
  `state.derive_status(&reviews, &policy)` →
  `status.work_for(author, role)`.
- `feedback.rs`: remove `--plan`, write to flat
  `feedback/<sha>.md`.
- `feedback_scan.rs`: simplify to scan flat directory.
- `disk_format.rs`: remove `FeedbackTarget`, simplify parsing.
- `stop_hook.rs`: render `AdHocReview` / `AdHocRevise`.
  Update existing plan reviewer prompt to remove `--plan`
  from the `clank feedback write` command.
- `finish.rs`: simplify. `clank finish` verifies approvals
  exist in flat feedback (gate check), then commits an empty
  file `.clank/finished/<plan>` (no directory, no extension).
  `finish_predicate_at` updated to check for a file instead
  of a directory.
- `preview.rs`: update gate checks to use flat feedback.
  Sealed-approval logic simplified (no copied feedback to seal).
- `disk_format.rs`: rename `parse_finalize_path` →
  `parse_finish_path`, update to accept `.clank/finished/<plan>`
  (file, no subdirectory).
- `git_io.rs`: `finish_predicate_at` updated to check for
  `.clank/finished/<plan>` file. Rename `parse_finalize_subpath`
  → `parse_finish_subpath`, update `diff_tree_changes` finish
  detection for new file shape. `tree_plan_paths` updated for
  rewrite stripping.
- `disk_snapshot.rs`: `enrich_with_newly_finished` works with
  the new finish candidate discovery.
- `preview.rs`: rewrite path predicates updated for file shape.
- `purge.rs`: strip paths updated for file instead of directory.
- `status.rs`: update `PlanView` building to use flat feedback
  (scan `feedback/<sha>.md` instead of `feedback/<plan>/<sha>.md`).
- `log.rs`: update `collect_reviews` to scan flat feedback path.
- `fs_watcher.rs`: `path_to_signal` updated to parse flat
  feedback path (no plan segment). `FeedbackWritten` signal
  triggers a full refold rather than plan-scoped wake.
- `runtime.rs`: remove plan-based feedback routing — just
  refold on any feedback write.
- `setup_assets/claude_skill.md` + `codex_skill.md`: update
  `clank feedback write` docs to remove `--plan`.
- Migration: move old plan-scoped files to flat layout.
- Lifecycle hooks: ad-hoc items skip `firings_from_items`.

## Tests

- Unreviewed ad-hoc → reviewer wfw returns work.
- Reviewer approves → no more work.
- Reviewer request_changes → master revise.
- `force_review_on_misc_commits=false`: no ad-hoc work.
- `ad_hoc_reviewers=["codex"]`: only codex gets ad-hoc work.
- `clank feedback write --commit <sha>` writes flat path.
- Plan feedback still works with flat path.
- `clank finish` checks approvals, commits empty
  `.clank/finished/<plan>` file.
- Stop-hook reviewer prompt uses `--commit` (no `--plan`).
- Migration moves old files correctly.
- Ad-hoc items don't fire lifecycle hooks.

## Acceptance criteria

- Feedback is at `.clank/agents/<author>/feedback/<sha>.md`.
- `derive_status` + `work_for` replaces `derive_work`.
- Ad-hoc commits surface as wfw work items when config enables.
- `clank feedback write` takes `--commit`, no `--plan`.
- Old plan-scoped feedback migrated to flat layout.
