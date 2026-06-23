# commit-tag-guidance

An agent got stuck on commit-tag rules: it committed with `[<plan>]` for a
plan that doesn't exist, the fix-commit-tag correction kicked in, and the
agent couldn't figure out the right fix. Two gaps caused it.

## Gap 1 — the master skill never documents commit tagging

`crates/cli/src/cli/setup_assets/skill_master.md` talks about "commit → STOP"
all over but NEVER explains HOW to tag a commit. An agent has no rule to
follow, so it guesses. Add a short, explicit "Commit tagging" section:
- A commit that advances a plan is tagged `[<plan>]` in its subject, and the
  tag must name an ACTIVE plan AND match the plan file(s) the commit touches.
- A commit that is NOT plan work (ad-hoc fix, scratch) takes NO tag.
- A tag that names no active plan, or doesn't match the touched plan files,
  is a VIOLATION — clank surfaces a `fix-commit-tag` correction and you amend
  the commit (re-tag, or DROP the tag if it was ad-hoc) before anything else
  proceeds.
- (Promotion/finish commits are tagged by clank's own commands — agents only
  tag their own implementation commits.)
Mirror the same rule into `skill_shared_core.md` if that's the shared home so
reviewers see it too.

## Gap 2 — the fix-commit-tag hint buries the ad-hoc option

`crates/cli/src/cli/wfw.rs` ~742 renders:
> `fix-commit-tag <sha> names no active plan: <tag> — amend the tag to EXACTLY
> the plan files the commit touches (no tag = ad-hoc)`

It LEADS with "amend the tag to the plan files," which presumes the commit
SHOULD be tagged; the "(no tag = ad-hoc)" escape hatch is a terse trailing
parenthetical. For the UNKNOWN-tag case (tag names no active plan) the common
truth is the agent meant an ad-hoc commit — so the hint should surface that
first. Reword so dropping the tag is an explicit, equal option, e.g.:
> `... [<tag>] names no active plan. If this commit isn't plan work, amend to
> REMOVE the tag (ad-hoc). Otherwise amend the tag to exactly the active
> plan(s) whose files it touches.`
Keep it terse (wfw-output-is-a-minimal-hint) but make the two branches
explicit. Check whether `status.rs` ~1055 (`MasterToFixCommitTag` reason)
renders a parallel string that should match.

## Out of scope

- Changing the tag INVARIANT or the correction state machine
  (commit-tag-fixup-is-first-class-state) — only the GUIDANCE (skill text) and
  the HINT wording.

## Note (installed skill templates)

`setup_assets/skill_*.md` are templates installed into repos' `.clank/skills/`.
Updating them means existing repos need a skill re-sync (the
install-breaking-binary playbook: re-run `clank init`/skill update across
repos after install) so live agents actually get the new guidance.

## Testing (no-binary-spawning)

- A wfw/wait unit test: the FixCommitTag hint for an unknown-tag violation
  contains the "amend to remove the tag (ad-hoc)" branch, not just the
  "amend the tag to the plan files" branch.
- (Skill text is prose — no test; verify it renders in a freshly-init'd repo.)

## Acceptance

- `skill_master.md` has a Commit-tagging section stating: `[<plan>]` for active
  plan work matching touched files; NO tag for ad-hoc; mismatch → amend.
- The fix-commit-tag hint presents "remove the tag (ad-hoc)" as an explicit
  option, surfaced for the unknown-tag case (not a buried parenthetical).
- status.rs's parallel reason (if any) is consistent.
