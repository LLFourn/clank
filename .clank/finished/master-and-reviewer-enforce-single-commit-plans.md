# master-and-reviewer-enforce-single-commit-plans
# Master and reviewer enforce single-commit plans

## Why

The current master workflow encourages small implementation commits even when a
plan has no justified internal review boundary. The user wants a complete plan
implemented in one commit by default, with reviewers correcting incremental work.

## Contracts

Update the canonical master and reviewer skill sources, their installed copies,
and any workflow documentation or setup-generated guidance that contradicts them.
Preserve the existing commit-then-stop review boundary, promotion and finalization
rules; a promotion commit is not an implementation milestone.

Add to the master skill:

> A plan MUST be implemented in a single commit unless the plan specifies internal
> milestones in the plan. Do not include internal named milestones in a plan
> without a strong justification.

Add to the reviewer skill:

> Master agents should try and implement everything a plan specifies in a single
> commit unless the plan specifies internal named milestones. A master agent that
> is implementing a plan with incremental commits is misbehaving and reviews MUST
> correct their behavior. A reviewer should be skeptical and request changes on
> plans with internal milestones that do not have a strong justification for them.

Do not retrofit named milestones merely to justify incremental commits. Explain
any exception concretely in the plan before implementation, with the necessary
review boundary and why a complete implementation commit would be unsuitable.
Implement this plan in one implementation commit.

## Validation and exit

Locate all shipped master/reviewer guidance and remove contradictory encouragement
to split implementation routinely. Verify setup installs the updated text, and
that the review loop still stops after commits. Install the reviewed skill changes.
Both roles explicitly enforce the complete-plan default and justified exceptions.

## Implementation and validation

The canonical master skill now includes the requested MUST rule and replaces
routine next-milestone instructions with complete-plan implementation. Exceptions
need a concrete justification for their review boundary before implementation;
milestones may not be retrofitted to excuse partial commits. Promotion,
finalization and requested review corrections retain their existing review loop.

The canonical reviewer skill includes the requested behavior-correction rule and
requires REQUEST_CHANGES for incomplete implementation without justified named
milestones and for plans whose milestones lack justification. CONTINUE now
describes accepted introductions and justified milestones with identified work
still owed, rather than routine unfinished implementation. FINISHED still means
the committable deliverables are implemented and merge-ready. Existing
architecture-first, role guard, external-step and commit-then-stop rules remain.

Setup's generated master description, README overview, diagram and quickstart
match the complete-plan default. Provider-specific wake mechanics are unchanged.
Reviewer wording was condensed to preserve the existing size ceiling rather than
raising the budget: the generated Codex reviewer skill is 5,684 bytes, below its
5,700-byte ceiling.

All 70 existing setup tests pass, covering provider composition, role boundaries,
review semantics, installation, upgrades and idempotence. The actual new CLI
installed skills into an isolated temporary home; the bundled skill validator
passed for master/reviewer in all four provider directories (eight skills).
Installed output contains both requested rules and retains stop-after-commit/
verdict instructions. A sweep finds none of the contradictory commit-by-commit
or routine next-milestone wording in shipped guidance. Diff whitespace and
formatting checks pass. User-profile installation is deferred until review.
