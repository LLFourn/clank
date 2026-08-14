# wakes-state-their-discharge-condition

## Problem

Agents are woken repeatedly with the same item and cannot tell WHY it
keeps firing or what would make it stop. Reported from live sessions:
they "don't seem to understand how to stop it" and "don't get any
feedback from the stop hook as to why they keep getting interrupted".

The wake says what to do and nothing else:

```
Clank wait returned work for `claude` (master). Items:
  - master: continue blocks-are-always-repo-wide @ 06194f0 (gate_continue)
```

That is a correct instruction and a useless explanation. An agent that
believes it already did the thing, or that cannot act on it, has no
signal that it is repeating and no way to work out what discharges it.

This is deliberate, not accidental. `render_wait_items_ref` records the
rule (`wait-output-is-a-minimal-hint`): "one line per item — WHO/verb +
plan + short sha. The HOW … lives in the agent's skill doc, not
re-taught per wake."

## The modeling error

That rule is right about SYNTAX and wrong about STATE. Re-teaching
`clank feedback write`'s flags on every wake would be noise — the skill
doc owns that. But "why is this firing again, and what ends it" is not
syntax; it is the one thing the agent cannot look up, because it
depends on the repo state that produced this item.

Level-triggering means an item fires whenever its condition holds. The
agent is never told what that condition IS. So the loop is invisible
from inside: each wake looks like a fresh instruction.

## Goal

Every wake states the state change that will stop it — derived from the
item, not from history.

## Approach

1. **Classify all 19 `WaitItem` variants — the core algebra, not the
   hook's rendered branches.** Two earlier drafts got the inventory
   wrong: the first listed 7 handpicked kinds, the second listed the
   hook's 15 render branches and folded `FixCommitTag` into `master`'s
   reasons. Both were wrong about the same thing — the set being
   classified was not the set that exists.

   The authority is `clank_core::wait::WaitItem`:

   `Master`, `Reviewer`, `Finished`, `Idle`, `AdHocReview`,
   `AdHocRevise`, `AdHocSettled`, `FixCommitTag`, `MultiplePlansOpen`,
   `PromoteFromQueue`, `GithubEvent`, `CommandEvent`, `ForCommit`,
   `ForFinished`, `ForBlocked`, `Blocked`, `Unblocked`, `PrReviewer`,
   `PrMaster`.

   Split them explicitly, because "delivered to an agent" and "exists"
   are different questions:

   - **Normal stop-hook wakes** — what an agent actually receives, and
     what this plan is for.
   - **Observer-only events** (`ForCommit`, `ForFinished`,
     `ForBlocked`) — watcher deliveries outside stop-hook wake
     delivery. Still classified explicitly, with that as the stated
     rationale, so "not applicable" is a recorded decision rather than
     an omission.

   Each normal wake is REPEATING (state the condition) or ONE-SHOT
   (state that it will not repeat, and why). A one-shot with no
   rationale is unexamined, not classified.

   Three that must not be guessed:

   - `FixCommitTag` is its OWN variant, not a `Master` reason.
     `work_for` emits it directly, `wait` serializes it as
     `fix_commit_tag`, and the stop hook has NO branch for it — it
     renders through the catchall today. So it is already a recurring
     wake arriving with no explanation, which is precisely this plan's
     complaint, sitting unnoticed in the code (codex on 4295908).
   - `GithubEvent` repeats until the event is explicitly acked; the ack
     IS its discharge condition.
   - `Master` carries several `WaitingReason`s — `gate_continue`,
     `address_commit_changes`, `ready_to_finalize`,
     `commit_plan_revision` — each discharged differently, so the
     condition keys on the REASON, not the kind.
   - `Blocked` is the one item an agent must NOT try to discharge; it
     says so, and names the human.

2. **Guard the classification against drift with a compile-checked
   exhaustive match.** A `match` over `clank_core::wait::WaitItem` in
   the classification (or a test helper mapping variants to expected
   kinds) means adding a variant WITHOUT classifying it fails the
   build. String matching on the hook's loose projection cannot give
   that, so the exhaustiveness has to be anchored on the core enum —
   otherwise a future kind silently renders with no condition, which is
   exactly today's bug arriving again.

2. **One line, appended to the existing item line.** Not a paragraph
   and not a re-teaching of command syntax — the skill doc keeps that
   job. The minimal-hint rule is amended, not discarded: minimal about
   HOW, explicit about WHAT ENDS IT.

3. **Say it when it can actually help.** The condition is most valuable
   on a repeat and merely noise on the first wake, but the hook is
   stateless and cannot tell them apart. Include it unconditionally
   rather than reintroducing a latch — one extra line per wake is
   cheaper than an agent looping blind.

## Required tests

In-process library tests (no binary spawning):

- **Table-driven over all 19 variants**: one case each, asserting the
  discharge condition, or the one-shot rationale, or the observer-only
  classification. The table IS the classification, so a variant cannot
  be tested into existence without being classified.
- **`fix_commit_tag` renders through a real branch**, not the
  catchall — it is a recurring wake and must carry its condition like
  any other.
- **A new core variant fails the build**: the exhaustive match over
  `clank_core::wait::WaitItem` is the guard; a test that constructs
  every variant proves it compiles today and breaks when one is added
  unclassified.
- **`master` keys on the REASON, not the kind**: `gate_continue` and
  `address_commit_changes` render DIFFERENT conditions from the same
  kind.
- **`github_event` names its ack** as the discharge — the repeating
  inbox delivery codex identified.
- **`blocked` tells the agent not to act**, and names the human as the
  only party who can clear it.
- **Unknown/future kinds stay fail-soft**: the catchall renders without
  a condition and does not panic, preserving
  `parse_wait_json_is_fail_soft_on_unknown_kind_and_extra_fields`.
- **The sha survives**: the 12-char sha an agent composes
  `feedback write --commit` from is still present on the line.

## Acceptance

- All 19 `WaitItem` variants are classified: a discharge condition, an
  explicit one-shot rationale, or an explicit observer-only rationale.
- No agent-delivered wake renders through the catchall.
- Adding a `WaitItem` variant without classifying it does not compile.
- No stored state is introduced anywhere in the hook.
- The `blocked` wake explicitly tells the agent not to act.

## Out of scope

- Suppression itself (`attending-suppresses-standing-wakes`, shipped).
  This plan does not reduce the number of wakes; it makes the ones that
  fire explicable.
- Re-teaching command syntax per wake. The skill doc owns HOW.
