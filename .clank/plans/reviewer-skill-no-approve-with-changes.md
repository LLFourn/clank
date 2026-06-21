# reviewer-skill-no-approve-with-changes

Close a verdict-disambiguation gap in the reviewer skill: a reviewer who
wants a change to a commit must use REQUEST_CHANGES — never APPROVE with
the change requested in the approval message. The `### Verdicts` section
of the reviewer skill defines what each verdict MEANS, but never states
this rule, so the failure mode is unwarned.

## Change — one sentence

Add a single sentence to the top of the `### Verdicts` section in
`crates/cli/src/cli/setup_assets/skill_reviewer.md` (before the
APPROVE/FINISHED/REQUEST_CHANGES bullets):

> **DO NOT** APPROVE a commit and request some changes in the approval
> message.

One sentence, not a paragraph — the bullets already define the verdicts;
this is just the disambiguator between APPROVE and REQUEST_CHANGES. If a
reviewer feels it needs a tail, "— use REQUEST_CHANGES instead" keeps it
one sentence, but the wording above is the intended minimum.

## Why

A reviewer who wants a small or optional change is tempted to APPROVE and
mention the change, then secretly gate FINISHED on it. That is worse than
either honest verdict: APPROVE tells the master "ship it," so if the
other reviewers also approve, the change never lands — while you believed
you had held the gate. This actually happened reviewing
`agent-set-review-tier` @ 41cb19b (APPROVE + "I'll mark FINISHED once the
type-tightening lands"). One explicit sentence makes the rule unmissable.

## Acceptance

- The sentence appears at the top of `### Verdicts` in `skill_reviewer.md`.
- `clank setup` re-renders it into
  `~/.claude/skills/clank-reviewer/SKILL.md` (verify the sentence is
  present in the installed skill).
- Setup skill-render tests still pass; clippy within budget (cli ≤30);
  fmt clean.
