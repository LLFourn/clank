# adhoc-default-off

`review.adhoc_feedback` currently defaults to `true`, so opening
clank on a brand-new repo immediately treats the latest commit as
ad-hoc and asks for a review of it — surprising and noisy for any
repo that hasn't yet adopted clank.

Flip the default to `false`. Users who want clank to review
plain commits opt in via `clank config review.adhoc_feedback set
true`.

## Changes

- `crates/cli/src/cli/config.rs:34` — `adhoc_feedback: true` →
  `false` in `ReviewConfig::default`.
- `crates/cli/src/cli/config.rs:171` — `KEY_CATALOG`
  `adhoc_feedback` entry: `default: "true"` → `"false"` so
  `clank config` listings stay truthful.
- `crates/cli/src/cli/mod.rs:51` — clap doc comment on
  `ReviewAdhocFeedback`: `default: true` → `default: false` so
  `clank config --help` stays truthful.
- `crates/cli/src/cli/config.rs:541` — flip the default-true
  assertion to default-false. Other config tests already write
  their own value and are unaffected.
- `crates/cli/tests/wfw_integration.rs` — most tests already
  write `adhoc_feedback:false` or call `disable_adhoc_review`;
  leave those alone (they're now redundant but harmless).
  Keep the helper for the explicit-intent reads.

## Tests to add

- A new wfw integration test: fresh repo with no `.clank/`
  config and one plain commit, run `clank wfw --timeout 1s`
  with reviewer identity, assert it exits with the empty-work
  timeout code and emits no AdHocReview item.
- Extend the existing `KEY_CATALOG` test (or add one) to assert
  the `adhoc_feedback` entry's `default` string matches the
  Rust default — wired through `ReviewConfig::default()` so the
  two cannot drift again.

## Out of scope

- The deeper "adoption gate" fix (ignore plain commits made
  before the first PlanIntro / before `.clank/` lands). The
  default flip kills the immediate UX issue; the adoption gate
  can be a separate plan if we still want clank to ignore
  pre-adoption history once a repo opts in.
- Renaming or restructuring `disable_adhoc_review`.
