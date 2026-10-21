# pr-review-mode-is-gone
# PR review mode is gone

## Why

Lloyd decided GitHub PR review mode is the wrong shape: there are better
ways to review a PR, and this one is welded to GitHub (draft-review
polling, `gh` incantations, a PR-scoped gate beside the plan gate). It is
deleted outright, not hidden: no alias, no deprecation shim.

## What goes

- `clank pr-review` (start / note / propose / abort / finish / status):
  `crates/cli/src/cli/pr_review.rs`, `PrReviewArgs`/`PrReviewCmd` and their
  arg structs in `cli/mod.rs`, dispatch, and the `message.rs` `-m -`
  wiring for `pr-review note`.
- `crates/core/src/pr_review.rs` and everything in `clank_core::wait` that
  exists only for it: `WaitItem::PrReviewer`, the PR-master item and
  `PrMasterNext`, `WaitItem::DraftChanged`, `PrReviewInput`,
  `PrReviewWorkState`, `PlanStateLookup::pr_reviews`,
  `WorkStatus::pr_reviews`, and the derive/emit code for them.
- `crates/cli/src/cli/github/draft.rs` (its only caller is pr_review) and
  `Transport::Draft`.
- `fork create --review` and `run_fork_with_review` (fork's callers use
  `run_fork` / `run_fork_pinned` directly).
- Status, TUI (`status_tui` derive/render/scroll/fixtures), stop-hook
  rendering and the `pr_reviews` JSON field that show PR-review rows.
- The `clank-pr-review` skill (`setup_assets/pr_review_skill.md`,
  `PR_REVIEW_SKILL_BODY`, its install target and tests), and every
  mention in README, docs/cli-audit.md, GROK-INTEGRATION.md, the other
  skills and help text.
- `tests/it/pr_review_integration.rs` and the PR-review cases in other
  test modules.
- `pr-reviews` in `repo_watch`'s wake dirs.

## What stays

- `fork create --pr N` and `open --pr N`: forking onto a PR's head is
  plain git and useful without the review mode. Their help must stop
  describing a review.
- The GitHub wake sources (`wait_events`, feed/coordinator/ci/timeline,
  `clank events`, the `clank-github` skill). They are a separate feature.

## Durable state already on disk

Removing a type does not remove the bytes that used it:

- **Event logs** may hold rows with transport `draft` and kind
  `draft_changed`. Loading a log that contains them must still work.
  `clank events list`/ack and the wait path have to skip or tolerate such
  rows, never fail on them. Fixture the OLD row bytes in a test (copy a
  real serialized row from before the change). Do not build the fixture
  through the deleted type.
- **`.clank/pr-reviews/` dirs** in existing repos become inert. Keep the
  `/pr-reviews/` ignore line in `init_facts` so they don't surface as
  untracked, with a one-line why. Nothing reads them.
- **Installed skills**: earlier `clank setup` runs wrote
  `<tool>/skills/clank-pr-review/SKILL.md`. Setup must remove that file
  (and its dir if empty) when it is clank's own install. Leave it alone
  if the user has edited it, using whatever ownership check setup already
  applies to the files it manages. Test both cases.

## Acceptance

- `clank pr-review …` is an unknown subcommand, and `fork create --review`
  is an unknown flag. Clap enforces both, so add no tests asserting the
  absence.
- `git grep -i 'pr.review\|pr_review\|PrReview\|DraftChange\|Transport::Draft'`
  over crates/, README.md, docs/ and the skills returns only:
  - the init_facts ignore line;
  - the setup stale-skill removal and its tests;
  - the old-row event-log fixture.
  List these hits in the finish message.
- The CLI audit inventory is regenerated (`CLANK_BLESS_AUDIT=1`), and the
  verdict table and README reference tests pass.
- These pass, with session env cleared:
  - `cargo test -p clank --lib`;
  - `cargo test -p clank-core`;
  - `env -u CLANK_AGENT -u CLAUDE_CODE_SESSION_ID -u CODEX_THREAD_ID cargo test -p clank --test it`
    for fork_integration, standby_wakes, agent_setup_acceptance,
    clank_dir_boundary and git_boundary.
