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

## wfw routing

`WaitingOn` gets a new variant or repurposes existing ones:
- `CommitGateState::Finished` (latest reviewable) →
  `WaitingOn::MasterToFinalize`.
- `CommitGateState::Approved` (latest reviewable) → currently
  this splits into `MasterToImplement`/`MasterToFinalize` via
  `touched_code`. New model: it's a single ambiguous state —
  master can commit more work, or ping the reviewer to mark
  FINISHED. Easiest is a new
  `WaitingOn::MasterToContinueOrAskForFinished`, but a less
  forking option is to keep `MasterToImplement` (rename it
  `MasterToContinue`) and drop the touched_code split.
- Other states unchanged.

Decision deferred to review: do we want a distinct
"reviewer-can-mark-finished" state for the reviewer side, or
do reviewers just decide on their own when they think a plan
is done?

## CLI surface

`clank feedback write ... --verdict finished -m "ship it"`.
Files on disk start with `FINISHED\n` (parallel to `APPROVE`
and `REQUEST_CHANGES`).

`clank feedback read` renders the new verdict in its output.

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
