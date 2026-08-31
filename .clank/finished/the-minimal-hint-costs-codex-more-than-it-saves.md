# the-minimal-hint-costs-codex-more-than-it-saves

Codex re-reads `clank-reviewer/SKILL.md` in full nearly every time it
is woken to review. Measured in the bound codex session
(`019e54b7`, running since 2026-05-23): **587 complete reads** of a
5,667-byte file — roughly **831k tokens** spent re-reading one
112-line document. The same session shows **780 compaction events**,
so the skill body could not have persisted even if the protocol
allowed it.

## This is codex's contract, not a bug

Codex injects a `<skills_instructions>` block into every session:

> Do not carry skills across turns unless re-mentioned.

> After deciding to use a skill, the main agent must open and read its
> `SKILL.md` completely before taking task actions. If a read is
> truncated or paginated, continue until EOF.

Every clank wake is a new turn whose task matches the
`clank-reviewer` description, so the skill is re-selected and, by
contract, re-read in full before codex may act. The transcript shows
it obeying the EOF clause literally — `sed -n '1,220p'`, `'1,240p'`,
`'1,260p'`, `cat`.

## The rule we wrote inverts for this adapter

`wait-output-is-a-minimal-hint` states that the HOW — feedback-write
form, verdicts, promote evaluation, unblock — "lives in the agent's
skill doc, not re-taught per wake". That is correct for claude, where
the skill is read once and stays in context.

It is FALSE for codex, and the cost runs backwards: rather than saving
a ~200-byte hint per wake, it forces a 5.6KB full-document read per
wake. We are paying 25× the thing we declined to send.

The rule is not wrong; applying it uniformly is.

## Change

RESULT: compression delivered ~8%, not the halving first implied —
455 bytes off the assets, composed codex skill 5,667 -> ~5,212, about
67k of the measured 831k tokens. Recorded because it re-weights the
argument: the recurring read is the cost, and only removing it is a
large win.

**1. Compress the skill assets.** `skill_shared_core.md` (1,281B) and
`skill_reviewer.md` (3,446B) carry their content in more prose than it
needs. This is COMPRESSION, not deletion: anything cut must be an
example or a restatement, never a constraint. Claude reviewers read
the same assets, so compression is the only safe lever there.

The rules that must survive do NOT all live in those two files. The
composed skill has THREE sources, and an implementer who edits only
the assets will not see two of them:

- `skill_reviewer.md` — the three verdicts, REQUEST_CHANGES over
  approve-with-notes, ARCHITECTURE-FIRST review, and never gating a
  verdict on a manual/external step.
- `compose_skill_with` — the ROLE GUARD, generated in code, absent
  from every asset.
- the `WORK_LOOP_*` constants — the anti-poll discipline, ALREADY
  per-tool: claude's says "NEVER run `clank wait` yourself", codex's
  says "NEVER poll". That divergence is precedent for change 2, not
  an inconsistency to normalise away.

**2. Let codex's composed skill be leaner than claude's — ALREADY
TRUE, and I should have checked before proposing it.**

Measured: claude's composed reviewer skill is 6,571 bytes, codex's is
5,667. Codex is already the lean one. The `/clank` slash-command block
(~900B) is appended for claude only, and the work loops are already
per-tool. Everything that remains is the normative inventory, which
must stay in BOTH compositions — so there is nothing left that codex
can drop.

Change 2 is therefore complete on arrival, with no code change. Kept
in the plan rather than deleted, because "the per-tool seam already
does this" is the answer to the next person who proposes it.

## The normative inventory

What compression may never touch, in EITHER composition. This is the
plan's source of truth; the tests assert it item by item.

- Review ONLY committable artifacts — the scope boundary itself.
- NEVER withhold CONTINUE or FINISHED waiting on a manual or external
  step — the rule that depends on that boundary. Distinct from it:
  asserting only this one leaves the boundary itself unprotected.
- Review ARCHITECTURE-FIRST.
- The three verdicts, and REQUEST_CHANGES rather than CONTINUE while
  gating on a change.
- FINISHED means implemented and merge-ready, not "the plan text is
  written".
- Reviewer-only scope: never finish / promote / roster commands.
- The role guard, generated in `compose_skill_with`, in no asset.
- The anti-poll discipline, in the per-tool `WORK_LOOP_*` constants,
  in no asset.
- The minimal `clank feedback write --commit <sha> --verdict <v>
  --author <label> -m "<msg>"` form.
- Exactly ONE verdict for the handed SHA, then STOP.

The last two are not reference material. `clank --help` cannot express
"one verdict for THIS sha, then stop", and it is the guard against
writing feedback files directly or running on into the next item.

Three of these live outside the asset files, which is why the tests
assert against `compose_skill(role, tool)` and never against the `.md`
sources.

## Rejected: inlining the operational core into codex's wake

The first draft proposed sending the `clank feedback write` form and
the verdicts in the wake itself, to make the wake self-sufficient.

It cannot work, and the plan's own analysis is why. Codex re-reads
because the TASK MATCHES THE SKILL'S DESCRIPTION, and a fatter wake
does not change that match. The skill would be read in full anyway, so
the inlined bytes are pure duplication on every wake — a change that
is strictly worse than doing nothing.

"Measure and report that it did not reduce reads" is not an acceptance
criterion. It describes installing a known-negative change and then
confirming it.

Recorded here because the idea is intuitive and will otherwise be
proposed again.

## The description is the only thing that stops a read

Nothing in this plan stops codex selecting the skill; it only makes
selection cheaper. The sole lever on selection is the skill
DESCRIPTION, which is what codex matches the task against.

Narrowing it so routine review wakes no longer match would eliminate
the reads outright — and would also mean codex reviews without ever
loading the reviewer contract. That trade needs its own plan and its
own evidence. It is not smuggled in here.

## Tests

- `compose_skill(Reviewer, Codex)` is under a byte ceiling of 5,300,
  against 5,667 before compression and 5,214 today. Codex re-reads the
  whole document every reviewing turn, so size is the recurring cost
  and therefore the acceptance criterion. Claude's is deliberately
  uncapped: read once and kept, its bytes are amortised.
- Every rule in the normative inventory above is present in BOTH
  composed reviewer skills, asserted PER RULE against
  `compose_skill(role, tool)` — NOT against the asset files. The
  inventory includes the `clank feedback write` template and the
  one-verdict-then-stop rule, so a compression pass cannot trade
  either for a `--help` call. The composition is what an agent reads, and it is
  the only level at which a rule lost from any of the three sources
  (assets, generated role guard, per-tool work loop) shows up.
- Claude's composed reviewer skill keeps everything it has today, so
  leaning out codex cannot quietly degrade the other reviewers.

## Out of scope

- The skill DESCRIPTION, which is the only thing that gates
  re-selection. Its own plan, with its own evidence.
- Wake inlining, rejected above.
- `clank-master`, 9,178B and read the same way by a codex master. Same
  fix, same shape; land it here only if it costs nothing extra.
