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

## Part 1 — collapse the classifier (strip the inheritance + ambiguity machinery)

The `classify` fold (clank-core `repo_state.rs`) is an 8-case engine
with hint inheritance, a `[misc]` special case, and a prefix-ambiguity
warnings system. Use this plan to **strip it to the invariant** (lloyd:
"get rid of all the crazy inheritance stuff"). A commit is attributed
to plan `X` iff it **touches `.clank/plans/X.md`** (lifecycle — keeps
intro/finalize/delete detection and plan-body edits) **or is tagged
`[X]` where `X` is a known plan**. Everything else → `AdHoc`.

REMOVE:
- `active_plan_hint` (field on `RepoState`, `ClassifierInputs`, and
  `next_active_plan_hint`) — the entire untagged-inherits-the-active-
  plan mechanism. This is why a bare post-intro commit folds to the
  plan today (the show-adhoc test needed `[bar]` to force ad-hoc).
- `TitlePrefix::Misc` — `[misc]` is NOT special. `misc` isn't a plan,
  so `[misc]` is just an unknown-plan tag → ad-hoc, and flagged at HEAD
  (Part 2). The ONLY ad-hoc opt-in is *no tag*.
- The prefix-ambiguity warnings `UnknownPlanPrefix`, `MissingPrefix`,
  `AttributionMismatch` (and `classify`'s `warnings` output entirely).
  We do NOT retroactively warn "this commit 10 back was ambiguous" —
  non-compliant history just folds as ad-hoc, silently. The live HEAD
  nag (Part 2) catches ~99% of mistakes when they're made.

KEEP (NOT part of this complexity):
- Touches-based attribution + the intro/finalize/delete lifecycle.
- `Warning::DanglingPlanRef` + `RepoState.warnings` + `RepoWarning` —
  a separate concern (a plan ref that dangles), surfaced by
  `clank open` and hashed into the cache. classify simply stops
  contributing warnings; the enum keeps `DanglingPlanRef`.

Resulting `classify`: `[X…]` → the known subset (unknown → ∅); no
prefix → ∅. No hint, no misc, no warnings. (Touches are applied in
`apply_commit` as today.)

### Consumers to update

- `preview.rs` replays classification with `known_plans` + the hint —
  drop the hint from the replay.
- `log.rs` matches `TitlePrefix` — confirm it compiles without `Misc`.
- `open.rs` warning display + the cache-hash over `fold.warnings` —
  unaffected in shape (only `DanglingPlanRef` remains).

### Blast radius — verify, don't assume

A master's UNTAGGED code commit is now ad-hoc, not part of the active
plan's gate — so each commit must be tagged `[plan]` to be reviewed
under that plan (agents already do; every gix milestone was
`[actually-replace-git-with-gix] …`). Audit derive_status, plan
timelines, and the review gate for anything that leaned on untagged
inheritance before landing.

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

### Guard — enforce ONLY when clank is adopted (lloyd)

The nag is enforced ONLY when clank's plan state is committed. If
`.clank` is gitignored/untracked, clank is a LOCAL GUEST on a repo with
its own commit conventions: the fold can't see the plans (they're not
in history), so it can't tell a real `[X]` from a typo and the host's
`[app]`/`[ci]` commits are legitimate. The flag MUST be off there.

Use the EXISTING notion — **`RepoState.adopted`** — do NOT invent a new
"is `.clank` tracked" check (lloyd). `adopted` is already set at the
first COMMITTED plan event and is false when there's no committed plan
history (i.e. `.clank` uncommitted), which is exactly this gate. Gate
the wfw flag on `state.fold.adopted`. The fold classification (Part 1)
is unaffected — pre-adoption everything is already ad-hoc; only the
ENFORCEMENT is gated.

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
- Guard: the SAME `[bar]` HEAD in a NOT-adopted repo (no committed plan
  history, `state.fold.adopted == false`) → no flag (local-guest mode).

## Non-goals

- A `[misc]` special tag — subsumed by "no tag = ad-hoc".
- A config allowlist of host prefixes — HEAD-scope handles host.
- Auto-amending commits (wfw surfaces it; master fixes).
- Rewriting deep history to fix old mistyped tags (HEAD only).
- Surfacing ad-hoc commits in status — shipped as
  [[show-adhoc-commits-in-status]].
