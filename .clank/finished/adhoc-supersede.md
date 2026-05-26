# adhoc-supersede

## Problem

`clank wfw` returns stale `AdHocRevise` items for old ad-hoc
commits even after newer commits (plan or ad-hoc) have landed
and been reviewed. This causes infinite loops when auto-mode
is on.

## Root cause

`derive_status` checks `self.ad_hoc.last()` for the latest
ad-hoc commit's gate, but `ad_hoc` only tracks commits with
no plan attribution. If the latest ad-hoc commit has
REQUEST_CHANGES and a newer plan commit lands after it, the
ad-hoc vec still has the old commit as its last entry because
plan commits don't clear or supersede the ad-hoc list.

## Fix

An ad-hoc commit is superseded when ANY newer commit exists
after it in the first-parent chain — whether that newer
commit is plan-attributed or ad-hoc. Once superseded, its
review state is irrelevant.

In `derive_status`: only check the last ad-hoc commit if it
is also the repo's most recent commit overall. If the repo
HEAD is newer than the last ad-hoc commit, the ad-hoc review
is stale and should not produce work items.

Alternatively: `RepoState` could track the latest commit SHA
overall, and `derive_status` compares `ad_hoc.last().sha`
against it.

## Tests

- Ad-hoc commit with REQUEST_CHANGES, then a plan commit
  lands → no AdHocRevise returned.
- Ad-hoc commit with REQUEST_CHANGES, then a newer ad-hoc
  commit lands → only the newer one matters (already works
  via `ad_hoc.last()`).
- Ad-hoc commit at HEAD with REQUEST_CHANGES → AdHocRevise
  returned (still works).
