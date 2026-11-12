# wake-messages-say-only-what-is-needed
# Wake messages say what happened and what to do, succinctly

## Why

Lloyd, 2026-11-12: the background-work check-in successfully woke Codex,
but its message was too verbose. Queue this after the current test cleanup.
The example repeated the pending master step, its discharge condition, an
instruction to act, elapsed and expected times, a renewal command, reassurance,
and a paragraph explaining attendance semantics.

## Change

- Audit the shared wake rendering, attendance check-ins and repeat reminders
  (`wake_render.rs`, `wake_repeat.rs`) across supported harnesses.
- Replace Codex setup's static `statusMessage` (currently "Clank: checking for
  pending review work") with concise role-neutral wording. This label comes
  from installed hook configuration, not the monitor, and is also shown to
  masters and standby agents. Verify setup refreshes existing installations.
- Put the event and actionable next step first. A routine check-in should be
  one or two short lines, for example:
  `Check-in: "fixture validation" — 10m elapsed (expected 10m).`
  `Inspect output; if still running: clank attend 'fixture validation' --expect 20m.`
- Keep the exact work name usable in the command. Distinguish check-ins from
  completion or failure, and runs from declarations where the action differs.
- Make pending plan/review items concise while retaining the role, action,
  plan, commit and any necessary discharge instruction. Avoid repeating the
  same instruction in the header, item and footer.
- Remove routine explanations of how attendance works; keep those in the
  skills/help. Repeated-delivery messages should add only useful new information.
  Preserve the constraints that repeated delivery does not grant authority,
  override an explicit pause, or bypass a rejected action.
- Change presentation only: no changes to wake eligibility, delivery, timing,
  acknowledgment, attendance lifetime or gating. Do not truncate user content,
  event payloads, questions or review findings.

## Validation

Use representative rendered examples for a single check-in, mixed pending work
and check-in, completion, review, and repeated delivery. Review the whole output
for brevity and confirm the needed identifiers and commands survive. Update
existing renderer tests around actionable content rather than freezing the old
boilerplate. Verify commands still quote work names safely and shared output
reaches each supported harness unchanged in meaning.

Implement in one commit after `tests-catch-bugs-without-flaking`.
