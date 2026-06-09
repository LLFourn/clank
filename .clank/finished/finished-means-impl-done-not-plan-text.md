# finished-means-impl-done-not-plan-text

The reviewer-facing guidance for the FINISHED verdict describes
it as "the plan is done" / "the work is complete" / "the
document itself is done". Agents read "the plan is done" as
"the plan TEXT is written" and mark FINISHED as soon as the
spec/plan doc looks complete — before any implementation
exists. FINISHED then triggers `clank finish`, finalizing a
plan whose code was never written.

## The confusing language

THREE surfaces emit this (the promote-time sweep found a third
beyond the two originally listed), and the wording conflates the
plan DOCUMENT with the plan's IMPLEMENTATION:

- `crates/cli/src/cli/stop_hook.rs:220` (wfw reviewer item):
  > Use FINISHED when you think the plan is done and `clank
  > finish` should run.

  "the plan is done" is ambiguous — done being written, or done
  being implemented?

- `crates/cli/src/cli/setup_assets/claude_skill.md:32-35`
  (the canonical skill doc):
  > FINISHED: this plan is done — `clank finish` should run.
  > Only mark FINISHED when you genuinely think the work is
  > complete (for a plan that's research, that's when the
  > document itself is done — no code change required).

  The research parenthetical actively teaches the wrong
  reading: doc-done == finished is the ONE exceptional case,
  but agents generalize it to every plan.

- `crates/cli/src/cli/setup_assets/codex_skill.md:~29` (the
  codex skill — mirror of the claude skill, found by the
  sweep): same "plan is fully done" conflation. Both skill
  assets must be reworded in lockstep so the claude and codex
  agents get the identical FINISHED definition. (Also note
  `claude_skill.md:29` / `codex_skill.md:29` carry a parallel
  "plan is fully done" phrase in the APPROVE guidance — fix that
  too.)

## The fix (direction, not prescription)

Make FINISHED unambiguously mean: the work the plan DESCRIBES
is fully implemented and merge-ready (code written, tests
passing, review satisfied) — NOT that the plan/spec text is
written. A complete plan document is the START of
implementation, not the end of it.

- Reword both surfaces to talk about the implementation/work
  being delivered, not "the plan being done".
- Keep the research/doc-only case, but frame it explicitly as
  the EXCEPTION ("if the plan's only deliverable is a document,
  the document IS the implementation") so it can't be read as
  the general rule.
- Grep for sibling copy with the same conflation:
  "the plan is done", "work is complete", "the document itself
  is done", and any verdict-help strings.

## Relationship to other queued work

`800-wfw-output-is-a-minimal-hint` plans to delete the
FINISHED-vs-APPROVE essay from the stop_hook/wfw output
entirely. If that lands first the `stop_hook.rs` copy may be
gone — but `claude_skill.md` remains the canonical home for the
FINISHED definition and is where the disambiguation must live
regardless. This plan owns the WORDING; that plan owns the wfw
payload SIZE.

## Out of scope

- Changing finish/gate mechanics. Wording/clarity only — no
  change to when `clank finish` is permitted.

## Status

Stub — queued (lloyd 2026-06-09: agents marking FINISHED on
plan-text-complete instead of implementation-complete).
