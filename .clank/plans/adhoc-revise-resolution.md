# adhoc-revise-resolution

## Problem

When a reviewer sends REQUEST_CHANGES on an ad-hoc commit, the
`AdHocRevise` work item persists indefinitely. There's no
natural "next commit" that supersedes the feedback — each ad-hoc
commit has its own independent gate. Master gets stuck in a loop
seeing the same revise item every wfw cycle.

Plan commits don't have this problem: the gate lives on the
*latest reviewable* commit, so a new plan commit advances the
gate target and the old feedback becomes irrelevant.

## Design

Ad-hoc commits that have been superseded by newer commits should
no longer surface as revise work. An ad-hoc commit is
"superseded" when any descendant commit exists — the author
has moved on. The REQUEST_CHANGES feedback is still recorded
(visible in `clank log`) but no longer blocks master.

### Rule

In `derive_status`, only the LAST ad-hoc commit with
`ChangesRequested` should produce an `AdHocRevise` item. All
earlier ad-hoc commits with changes requested are considered
addressed by the subsequent work. Similarly, an ad-hoc commit
with `ChangesRequested` that has ANY later commit after it
(ad-hoc or plan) is considered addressed.

Simpler: only surface `AdHocRevise` for the most recent
ad-hoc commit. If it's approved or unreviewed, no revise work.
If it has changes requested, surface it. Earlier ad-hoc commits
are history.

### Implementation

`RepoState.ad_hoc` is a `Vec<AdHocEvent>` in chronological
order. `derive_status` currently iterates ALL of them and
checks each gate. Change to only check the last one (or the
last N that are still at HEAD — but "last" is simplest).

Actually, even simpler: ad-hoc review should only apply to the
most recent ad-hoc commit that hasn't been followed by a plan
commit. Once a plan commit lands, earlier ad-hoc commits are
in the past.

Wait — `RepoState.ad_hoc` is already cleared on plan intro
(`self.ad_hoc.clear()` in `TouchKind::Intro`). So ad-hoc
commits only accumulate between plan intros. The issue is
within that window: if there are multiple ad-hoc commits,
only the latest should be reviewable.

### Change

In `derive_status` (`crates/core/src/wait.rs`), instead of
iterating all `self.ad_hoc`, only consider the last entry.
An approve on the latest ad-hoc commit implicitly approves
all earlier ones in the chain.

## Surface

- `crates/core/src/wait.rs`: `derive_status` only checks
  `self.ad_hoc.last()` for ad-hoc work.
- Test: REQUEST_CHANGES on ad-hoc commit, then new ad-hoc
  commit → old revise item gone.
- Test: approve on latest ad-hoc → no work for any earlier
  ad-hoc commits.
