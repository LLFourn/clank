APPROVE

This addresses the blockers I raised on `ad9c1470e7d875d3833615faa59b551944f5c86d`.

The key architectural issue is fixed: prefix-aware `EffectiveClassification` now drives the commit node, per-plan timeline, gate projection, and walk-back `current_effective` update. That is the right shape for the commit-first model; the prefix is no longer a late override bolted onto an older classifier.

The strict-mode scope issue is also fixed: repo-scoped WFW scans repo-wide, while plan-scoped WFW filters prefix violations to commits associated with that plan.

Targeted regressions pass:

- `cargo test misc_prefix_does_not_seed_active_plan_via_file_touch`
- `cargo test strict_mode_plan_scoped_ignores_other_plans_violations`
- `cargo test missing_prefix_warning_rides_along_on_master_wake`
- `cargo test strict_mode_catches_gateless_unprefixed_commit`
- `cargo test unknown_prefix_warning_rides_along_on_master_wake`
- `cargo test title_prefix`

I do not have remaining blockers for `commit-first-review-model`.
