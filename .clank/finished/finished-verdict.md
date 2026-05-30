# finished-verdict

Replace the structural "did the approved commit touch code?"
gate (`ImplementationNotApproved`) with a first-class reviewer
signal: a `FINISHED` verdict. APPROVE means "this commit's work
is good"; FINISHED means "this plan is done — finalize away."
Reviewers decide; clank doesn't infer.

This subsumes:
- The `finish-requires-code-approval` change (the
  `ImplementationNotApproved` block reason and the touched-code
  check it added become redundant).
- The research-cycle plan (research = a plan whose reviewer
  marks FINISHED on a plan-text commit; no separate workflow,
  no `.clank/research/` tree).

## Model

Add `Verdict::Finished` alongside the existing
`Verdict::{Approve, RequestChanges, Unmarked}` in
`crates/core/src/vocab.rs`.

Gate semantics on a commit's review set:
- Any `RequestChanges` → `ChangesRequested` (unchanged).
- Any `Finished` (no RequestChanges) → new
  `CommitGateState::Finished`.
- Any `Approve` (no RequestChanges, no Finished) → `Approved`
  (unchanged).
- Otherwise → `Unreviewed` (unchanged).

`Finished` is a strict superset of `Approve` — a FINISHED
verdict implies the commit's work is also good. We do NOT
require a separate APPROVE alongside the FINISHED.

## Finalize gate

`clank finish <plan>` is ready iff:
- Latest reviewable commit's `CommitGateState` is `Finished`.
- Plan worktree is clean (unchanged).
- Plan is not already finished (unchanged).

Concretely: replace
`FinalizeBlockReason::ImplementationNotApproved` with
`FinalizeBlockReason::NotFinished { state }` whose message is
something like "no reviewer has marked this plan FINISHED yet
(latest gate: <state>)".

The "touched_code" check goes away entirely. Research plans
work because their reviewer marks FINISHED on the plan-text
commit; the gate doesn't care whether code was touched.

## wfw routing (resolved)

`WaitingOn` is rewired around the new gate:
- Gate `Finished` on latest reviewable → `WaitingOn::MasterToFinalize`.
- Gate `Approved` on latest reviewable → `WaitingOn::MasterToContinue`
  (rename of the existing `MasterToImplement`). The
  `touched_code` split is dropped — master keeps working,
  whether that's more impl, more tests, more plan-text, or
  prompting the reviewer to mark FINISHED.
- Gate `ChangesRequested` → `MasterToRevise` (unchanged).
- Gate `Unreviewed` → `FirstReview` (unchanged).
- Reviewer side: when a reviewer's latest review on the
  latest reviewable commit is APPROVE (not FINISHED, not
  request-changes), they are NOT routed to do anything — the
  ball is in master's court. They can voluntarily upgrade
  their APPROVE to FINISHED at any time, but wfw doesn't
  prompt for it.

Knock-on renames: `MasterNext::Implement` →
`MasterNext::Continue`; `WaitingReason::ReadyToStartImplementation`
→ `WaitingReason::GateApproved` (or similar — the
reviewer-facing reason string stays neutral about what to do
next). All hard-coded strings in stop-hook prompts and
integration tests get updated.

## APPROVE-without-FINISHED reviewer etiquette

When a reviewer marks APPROVE but not FINISHED, the body must
briefly say WHY the plan isn't finished yet — one short
sentence (e.g. "tests still missing", "spec coverage looks
good, impl pending"). This is reviewer discipline, not a hard
validation: clank doesn't fail commits on missing prose. But
the rule is documented in:
- `crates/cli/src/cli/setup_assets/claude_skill.md`
- `crates/cli/src/cli/setup_assets/codex_skill.md`
- The reviewer half of the stop-hook prompt
  (`crates/cli/src/cli/stop_hook.rs`).

The skill text added is roughly: *"When you approve but don't
think the plan is fully done, include a one-sentence reason
the work isn't FINISHED — e.g. 'tests still missing.' This
keeps master oriented on what's left."*

## Surfaces touched (full list)

Core:
- `crates/core/src/vocab.rs` — add `Verdict::Finished` and
  `CommitGateState::Finished`. Update `as_str()` for both.
- `crates/core/src/wait.rs::compute_gate` — `Finished` votes
  resolve to `CommitGateState::Finished` per the precedence
  in "Model".
- `crates/core/src/wait.rs` + `crates/core/src/plan_view.rs` —
  rewire `WaitingOn` per "wfw routing (resolved)".
  `WaitingOn::MasterToImplement` → `MasterToContinue`;
  remove the `touched_code` branch in `evaluate`.
- `crates/core/src/api.rs` — drop
  `FinalizeBlockReason::ImplementationNotApproved`; add
  `FinalizeBlockReason::NotFinished { state }`.

CLI:
- `crates/cli/src/preview.rs::compute_finalize_readiness` —
  drop `latest_reviewable_touched_code` arg; gate on
  `CommitGateState::Finished`.
- `crates/cli/src/cli/finish.rs::reason_to_msg` — new arm for
  `NotFinished`.
- `crates/cli/src/cli/mod.rs::VerdictArg` (clap enum on
  `feedback write`) — add `Finished` variant.
- `crates/cli/src/cli/feedback.rs` — writer emits
  `FINISHED\n...` header; reader maps it back.
- `crates/cli/src/fs_review_lookup.rs` (or wherever feedback
  body parsing lives) — recognize the `FINISHED` first-line
  header. Unrecognized headers are still treated as `Unmarked`.
- `crates/cli/src/cli/log.rs` — render `FINISHED` in both
  oneline and default outputs; new color/symbol consistent
  with existing approve/request-changes marks.
- `crates/cli/src/cli/status.rs` — render the new gate state
  in human + JSON.
- `crates/cli/src/cli/stop_hook.rs` — reviewer prompt enumerates
  approve | finished | request-changes; master prompt for
  gate-approved-not-finished reads "approved — continue or
  ask for FINISHED."
- `crates/cli/src/cli/setup_assets/claude_skill.md` and
  `codex_skill.md` — document `--verdict finished` and the
  APPROVE-without-FINISHED brief-explanation rule.

## Acceptance criteria

- `clank feedback write --verdict finished --commit X -m "msg"`
  writes a file starting with `FINISHED\n` and the message
  body; `feedback read` echoes the verdict back as
  `FINISHED`.
- A plan whose latest reviewable commit has only APPROVE
  votes (no FINISHED) cannot be finalized; the error message
  references the new `NotFinished` reason and the current
  gate state.
- A plan whose latest reviewable commit has at least one
  FINISHED vote finalizes successfully, regardless of
  whether the commit touched code (research path).
- A request-changes vote on the latest reviewable commit
  beats both APPROVE and FINISHED on the same commit; gate is
  `ChangesRequested`.
- `clank status` shows the new gate state in human and JSON.
- `clank log` shows `FINISHED` as a distinct mark.
- `clank wfw` returns `MasterToContinue` (or whatever the
  renamed variant is called) for an approved-not-finished
  plan, and `MasterToFinalize` only when the gate is
  Finished.
- Stop-hook reviewer prompt mentions FINISHED as a valid
  verdict.
- Skill assets document the verdict and the
  APPROVE-without-FINISHED reason rule.

## Tests

- Unit: `compute_gate` precedence (request-changes >
  finished > approve > unreviewed). One test per ordering.
- Unit: `compute_finalize_readiness` blocks on
  `CommitGateState::Approved` with `NotFinished` reason;
  passes on `CommitGateState::Finished`; preserves all other
  existing cases.
- Unit: `evaluate` (plan_view) maps the four gate states to
  the four WaitingOn states.
- Integration: `clank feedback write --verdict finished`
  round-trips through `clank feedback read`.
- Integration: replay the existing
  `wfw_master_code_only_approval_routes_to_finalize` test
  semantics — now approve-only routes to `MasterToContinue`,
  finished routes to `MasterToFinalize`.
- Integration: `clank finish` fails on an approve-only chain
  with a NotFinished error; succeeds on a finished chain
  (including a plan-text-only chain — the research path).
- Integration: stop-hook reviewer prompt output contains
  `finished` as a verdict option.

## CLI surface (summary)

`clank feedback write ... --verdict finished -m "ship it"`.
Files on disk start with `FINISHED\n` (parallel to `APPROVE`
and `REQUEST_CHANGES`). `clank feedback read` echoes the new
verdict back.

## Migration / compatibility

The old `ImplementationNotApproved` variant is removed.
Existing feedback files using APPROVE keep working — they just
don't gate finalize anymore. Anyone whose habit was "approve
the impl, then clank finish" needs to switch to "finished the
impl." This is a behavior change but it's a strict win in
clarity.

## Out of scope

- A `RESEARCH` verdict or any research-specific tooling.
  Research plans work by convention: the body says so, the
  reviewer marks FINISHED when the doc is done.
- Multi-reviewer "consensus" gating (e.g. requiring N
  FINISHED votes). One FINISHED is enough.
- Per-commit FINISHED expiration. If reviewer Finished commit
  X and master pushes Y, the gate on Y is recomputed from Y's
  own reviews — old FINISHED on X doesn't carry forward.
