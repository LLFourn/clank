# adhoc-commits-and-plan-tag-validation

Make the commit `[tag]` the single source of a commit's plan
association, and catch mistyped tags.

## The invariant (lloyd)

> A commit's `[tag]` IS its plan association.
> - `[X]` present ⟹ `X` must be an active plan, else it's a mistake.
> - No tag ⟹ ad-hoc.

This replaces today's implicit `active_plan_hint` inheritance (a bare
commit after a plan's intro is currently attributed to that plan). It
also subsumes the earlier `[misc]` idea — the ad-hoc opt-in is simply
*not tagging*, so there's no special tag to add.

## Part 1 — fold: the tag is the association (drop hint-inheritance)

Change `apply_commit` (clank-core `repo_state.rs`) classification:
- tagged `[X]` where `X` is an active plan → `PlanCommit(X)` (as today
  via touches/tag);
- EVERYTHING ELSE — untagged, OR `[X]` where `X` is not an active plan
  → `AdHoc` (post-adoption, as today).

Concretely: stop attributing untagged commits to the active plan via
`active_plan_hint`. (That hint is exactly why a bare post-intro commit
folds to the plan today — the show-adhoc reproduction test had to use a
`[bar]` tag to force ad-hoc.)

### Blast radius — verify, don't assume

The fold's attribution feeds status, log, AND reviews. Dropping
inheritance means a master's UNTAGGED commit is now ad-hoc, not part of
the active plan's gate — so a master must tag `[plan]` on each commit it
wants reviewed under that plan (agents already do; every gix milestone
was `[actually-replace-git-with-gix] …`). Audit the in-process flows
(derive_status, plan timelines, the review gate) for anything that
relied on untagged inheritance before landing this.

## Part 2 — wfw flags a mistyped tag

`wfw` (master role) inspects the commit master JUST made (HEAD): if it's
`[X]` where `X` is not an active plan, return a work item — "commit
`<sha>` prefix `[X]` isn't an active plan — amend to a real plan tag, or
drop the tag to make it ad-hoc."

**HEAD-scope is the whole host-prefix answer.** frostsnap's `[app]`,
`[ci]`, `[coord]`, … are `[..]`-not-a-plan tags too, but they're
human/CI commits in history (or merged-in ancestors) — never the commit
master just made. Checking only master's fresh HEAD means host
conventions are never flagged. No config allowlist, no since-intro
bookkeeping needed.
- Resolution = amend HEAD's message. (Amending deeper history needs a
  rewrite — out of scope; only HEAD is flagged.)

### Guard — enforce ONLY when `.clank` is committed (lloyd)

The nag is enforced ONLY when `.clank` is committed/tracked in git. If
`.clank` is gitignored or untracked, clank is a LOCAL GUEST on a repo
with its own commit conventions: the fold can't see the plans (they're
not in history), so it can't tell a real `[X]` from a typo, and the
host's `[app]`/`[ci]` commits are legitimately tagged. In that mode the
wfw flag MUST be disabled — never police a repo whose clank state isn't
committed. (This guards the case HEAD-scope alone doesn't: a repo where
the operator runs clank locally without committing `.clank`.)

Detection: `.clank` is tracked in git (the standard `clank init` layout
commits `plans/`+`finished/`, gitignores `cache/`+`queue/`). If nothing
under `.clank` is tracked, skip the flag entirely. The fold
classification (Part 1) is unaffected — with no committed plans
everything is already ad-hoc; only the ENFORCEMENT is gated.

## Testing (reproduce-first; in-process; no binary spawning — [[no-binary-spawning-tests]])

- Fold classification (the core change):
  - tagged `[foo]` (foo active) → `PlanCommit(foo)`;
  - untagged post-intro commit → `AdHoc` — **reproduce-first**: this
    folds to `PlanCommit(foo)` today (inheritance), so the test must
    flip from inherited→ad-hoc with the change;
  - `[bar]` (no such plan) → `AdHoc`.
- `wfw`: master's fresh HEAD `[bar]` (not a plan) → yields the
  fix-commit work item; `[foo]` → none; untagged → none; a `[app]`
  ANCESTOR (not HEAD) → none.
- Guard: the SAME `[bar]` HEAD in a repo where `.clank` is NOT tracked
  → no flag (local-guest mode).

## Non-goals

- A `[misc]` special tag — subsumed by "no tag = ad-hoc".
- A config allowlist of host prefixes — HEAD-scope handles host.
- Auto-amending commits (wfw surfaces it; master fixes).
- Rewriting deep history to fix old mistyped tags (HEAD only).
- Surfacing ad-hoc commits in status — shipped as
  [[show-adhoc-commits-in-status]].
