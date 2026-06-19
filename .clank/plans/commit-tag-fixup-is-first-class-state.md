# commit-tag-fixup-is-first-class-state

Enforce one invariant — **a commit's `[..]` tag must match the plan
files it touches** — and model a violation as a real, derived workflow
state instead of a master-only side-check, so `clank status`, the
reviewer tier, and the master all read the same "fix the commit tag"
correction from one place.

## The invariant (the model)

The plan-file-touch heuristic is the AUTHORITY for which plans a commit
belongs to; the `[..]` tag must declare it and is enforced to agree:

- Let `T` = the set of plans whose `.clank/plans/<x>.md` the commit's
  diff touches (the existing `plan_touches` heuristic, including
  intro/revise/finish/delete and cross-stem renames).
- Let `G` = the plan set named by the commit's `[..]` tag.
- **Every plan in `T` MUST appear in `G`** (`T ⊆ G`). Touch one plan
  file → `[that-plan]`. Touch two → `[plan1,plan2]`. This is the
  direction that does not exist today.
- **Every name in `G` MUST resolve to a real plan** — active, or
  introduced by this very commit (`G ⊆ active ∪ introduced`). This is
  what `head_tag_fixup` already checks.
- A commit that touches NO plan file (`T = ∅`) may be untagged
  (→ ad-hoc) or tagged with active plan(s) it attributes code to
  (implementation commits — the common case, must stay valid).

Consequences of the invariant:
- A commit that touches plan files and only plan files must have
  `G == T` exactly.
- `[plan]` touching `a-particular-plan.md` → `T={a-particular-plan}`,
  `G={plan}` → VIOLATION (the placeholder bug that motivated this).
- untagged touching `a.md` → `T={a}`, `G=∅` → VIOLATION.
- `[a]` touching `a.md`,`b.md` → `b ∉ G` → VIOLATION (must be `[a,b]`).
- legit `[foo] finish`/`delete` → the rename/delete IS a touch of `foo`,
  so `T={foo}=G` → fine (the existing carve-out falls out for free).

## Why it's broken today

Two independent attribution sources are UNIONED and never reconciled:
- tag via `classify` → `plan_attribution` (repo_state.rs:417-427)
- plan-file touch via `plan_touches` (git_io.rs:986-1126)
unioned into `affected` (repo_state.rs:502-505); `reviewable_shas =
touched_plan || touched_code` (repo_state.rs:93-99). So a plan-file touch
makes a commit reviewable for that plan EVEN WHEN the tag names a
different plan or none.

The only consistency check, `core::wait::head_tag_fixup`, validates just
`G ⊆ active ∪ touched` — never `T ⊆ G`. And it is a master-only
preemptive side-check: `wfw.rs::master_head_fixup` is its ONLY non-test
caller (wfw.rs:106-115, 224-234, 388-395). It is absent from
`derive_status`, `CommitGateState`, `WaitingOn`, and `status.rs`. So a
violation:
1. is invisible to `clank status` (derives from `derive_status`);
2. does not gate the reviewer — the reviewer's `wfw` has no check and the
   commit is `reviewable`, so the reviewer is woken and the gate can
   advance on a bad tag (confirmed: `wfw --role reviewer` returns a
   normal reviewer item for the `[plan]` commit);
3. does not even proactively wake the master — `FixCommitTag` yields no
   `HookFiring` (wfw.rs:86 → None), unlike `Reviewer`/`MasterWork`, so
   the bad commit proactively wakes the REVIEWER while the master
   correction sits passive until the master self-polls.

Observed in `frostsnap/.clank/worktrees/fix-signet-electrum`: HEAD
`0ec71e7b [plan] Fix electrum...` on plan `fix-electrum-disable-and-startup`
shows `gate: unreviewed / waiting on codex` with no hint the tag is wrong.

## Fix direction

1. **Generalize the check to the resolved invariant** (see "Resolved"
   below): replace `head_tag_fixup`'s one-directional test with
   `G == T` when `T ≠ ∅` (tag exactly the touched plan files) and
   `G ⊆ active ∪ introduced` always (for `T = ∅`, the existing
   untagged/active-tag rule stands). Report all three violation kinds:
   unknown tag names (`G ⊄ active∪introduced`), touched-but-unnamed
   plans (`T ⊄ G`), and named-but-untouched plans on a plan-touching
   commit (`G ⊋ T`). Keep adopted-gated + HEAD-only semantics.
2. **Make it a derived state, not a side-check.** In `derive_status`,
   when HEAD violates the invariant, yield a dominating state (e.g.
   `WaitingOn::MasterToFixCommitTag`, precedence like `Blocked`) that:
   - PRECEDES review so reviewers are NOT woken until the tag is fixed,
   - renders in `clank status` as a visible correction state,
   - is the SINGLE source for the master's `wfw` item — delete the
     bespoke `master_head_fixup`,
   - emits a proactive master `HookFiring` so the master is pulled back
     to amend rather than waiting on an idle self-poll.
3. **Collapse the dual source of truth.** With the tag enforced to match
   the file-touch set, the two attribution paths agree by construction;
   `reviewable_shas`/attribution can stop OR-ing a possibly-contradictory
   tag against the file touch.

## TUI display (`clank status --tui`)

The correction must be visible in the TUI, distinctly from normal work:
- Add `WaitingOn::MasterToFixCommitTag` arms to `emoji_of` and `verb_of`
  (status_tui.rs:600-618) — e.g. ⚠️ / "fixing tag" — so the plan row
  reads as a warning, not a routine master action.
- Color it ORANGE to signal a warning. `state_color` (status_tui.rs:526)
  currently emits 16-color SGR codes (`31` red, `33` yellow, `2` dim);
  true orange needs a 256-color code (`38;5;208`). Route it through a new
  `AttentionState` branch (e.g. `NeedsCorrection`) rather than hardcoding
  the hue at one call site, since `attention_state`/`state_color`
  (status_tui.rs:464-534) is also the SINGLE source for the zellij tab
  attention indicator — the bar lamp and the tab must not disagree
  (per the existing module note). Precedence: above `Active`, below
  `Blocked` (a human-blocking issue still dominates).

## Resolved: strict `G == T` when `T ≠ ∅` (lloyd)

Mixed commits that edit plan-a.md AND "carry code for active plan-b":
**enforce strict `G == T` for any commit that touches a plan file**, and
split otherwise. Relax later only if it proves painful.

The reasoning that settles it: clank CANNOT identify "code for plan-b" —
it does no diff-to-plan semantic analysis. The only signals are `T`
(which `.clank/plans/*.md` the diff touched — objective ground truth)
and `G` (the tag — an author claim). So "carries code for plan-b" just
means "the author wrote `[plan-b]`", which is unverifiable. Allowing
`G ⊋ T` would let an unverifiable extra tag name ride along on an
otherwise-verifiable planning commit. Therefore:

- **`T ≠ ∅` (touches a plan file):** require `G == T` — tag EXACTLY the
  touched plans, no extras. Genuine plan-b code goes in its OWN commit.
- **`T = ∅` (pure code / ad-hoc):** unchanged — untagged (ad-hoc) or
  tagged with active plan(s) (the usual unverifiable implementation
  attribution). `G == T` does NOT apply here (it would wrongly ban
  implementation tags).

Philosophy (lloyd): start strict, relax if needed; a violation is
surfaced (the `MasterToFixCommitTag` state) and fixed, never tolerated.

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- `derive_status` over a fold whose HEAD violates the invariant in each
  way (`[plan]` placeholder; untagged-but-touches; `[a]` while touching
  `a.md`+`b.md`) → MasterToFixCommitTag; reviewer `work_for` yields
  nothing; status renders the correction.
- `[a,b]` touching `a.md`+`b.md`, legit `[foo] finish`/`delete`, and
  untagged code-only ad-hoc all still pass (extend the cases at
  wait.rs:935-976).
- After the master amends to the matching tag, normal review resumes and
  the reviewer is woken.

## Non-goals

- Rewriting commit-tag parsing (`parse_subject`/`classify`) — reuse it.
- History policing — HEAD-only, adopted-gated semantics stay as today.
