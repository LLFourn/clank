# suppress-preadoption-adhoc

clank html's timeline (and any other LogEvent consumer)
shows a flood of `AdHoc` events for commits that landed
before this repo adopted clank. The fold at
\`crates/core/src/repo_state.rs:590\` emits
\`LogEvent::AdHoc\` for every code-only commit with no plan
attribution, regardless of whether the repo has ever
introduced a plan.

The cheap config flip (\`review.adhoc_feedback = false\`)
silences ad-hoc as REVIEW work but doesn't keep these
events out of the log. They still render in
\`clank log\`, \`clank html\`, and anything else that
consumes \`LogEvent\`.

## Fix

In \`RepoState::apply_commit\`, gate the AdHoc emission on
adoption: skip pushing the \`LogEvent::AdHoc\` (and the
\`ad_hoc\` bucket entry) when no plan has ever existed in
this repo's history at this point in the fold. Concretely:
skip iff \`self.plans.is_empty() && self.finished_plans.is_empty()\`
at the time the commit is being processed — after the
classifier has run but before the AdHoc push at line 590.

Once any plan intro lands (or any prior finalize is on the
chain), AdHoc resumes — those are the legitimate "post-
adoption stray commit" signals the bucket exists for.

## Surfaces touched

- \`crates/core/src/repo_state.rs::apply_commit\` — wrap the
  AdHoc push branch in an \`is_pre_adoption()\` check.
- Maybe a tiny helper \`fn pre_adoption(&self) -> bool { self.plans.is_empty() && self.finished_plans.is_empty() }\`
  for readability and reuse.

## Tests

- \`adhoc_suppressed_before_first_plan_intro\` — fold a repo
  with three plain commits then a \`[foo] intro\`; assert
  the log events for the first three are NOT AdHoc.
- \`adhoc_emitted_after_first_plan_intro\` — fold the same
  shape then add a plain commit AFTER the intro; assert
  that commit IS an AdHoc event.
- \`adhoc_emitted_again_after_all_plans_finished\` — intro,
  finish, then a plain commit; assert it IS AdHoc (there
  IS a finished plan in history, so we've adopted).
- Existing fold tests stay green — adoption only changes
  the pre-adoption case.

## Out of scope

- Backfilling existing repos. The fix is in the fold; once
  shipped, \`clank html\` and friends re-render cleanly on
  next run.
- Changing the \`ad_hoc\` BUCKET semantics for the
  \`derive_status\` path. That bucket is already
  policy-gated by \`review.adhoc_feedback\`; this fix is
  about what enters the LOG event stream.