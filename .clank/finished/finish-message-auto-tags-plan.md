# finish-message-auto-tags-plan

`clank finish` must ensure the finish commit's subject carries the plan's
`[<stem>]` tag, even when the user supplies a custom `-m`. Otherwise every
finish trips `fix_commit_tag`.

## Problem (a regression from finish-requires-plan-summary-message)

The finish commit always touches the plan's file (it renames
`.clank/plans/<stem>.md` → `.clank/finished/<stem>.md`), so clank's commit-tag
validation requires it to be tagged `[<stem>]`. The old default message
`[<stem>] finish` carried that tag for free. Now that finish REQUIRES a
custom `-m` (finish-requires-plan-summary-message), a message like
`-m "require a validated message" -m "<why>"` produces a subject with NO
`[<stem>]` tag → `fix_commit_tag` fires on the finalize commit.

This was hit finalizing that very plan (commit had to be hand-amended to add
the tag). Because `-m` is now mandatory on finish, this bites EVERY finish
unless the user manually prefixes the tag — a real UX regression.

## Fix

`clank finish` ensures the `[<stem>]` tag is present on the message that
lands on a plan-file-touching commit, idempotently:

- A pure helper `ensure_plan_tag(message: &str, stem: &str) -> String` that
  prepends `[<stem>] ` to the SUBJECT when it doesn't already start with
  `[<stem>]` (leave a user-supplied tag as-is; never double-tag). Only the
  subject line is touched; the body is untouched.
- Apply it when stamping the finalize/amend commit (`commit_message`) — i.e.
  in `finalize` / `amend_already_finished`, or once in `run()` before those
  calls. The result is that `clank finish <plan> -m "<subject>" -m "<why>"`
  lands `[<stem>] <subject>` + body.
- The `--squash` MSG lands on the collapsed commit, which still carries the
  finalize rename (`plans/<stem>.md → finished/<stem>.md`) → touches the plan
  file → the commit-tag rule requires `[<stem>]` (ruthless 28e3be4 verified
  the chain). So route the squash MSG through `ensure_plan_tag` too — a
  DEFINITE step, done in `run_post_finalize_rewrite`. The `--amend` path
  authors a plan-file-touching commit as well, so its `commit_message` is
  tagged the same way.

Ordering vs validation: `validate_finish_message` already strips a leading
`[<stem>]` for its placeholder check, so it accepts a message whether or not
it's tagged — tagging can happen after validation without interfering.

## Tests

- `ensure_plan_tag`: prepends when the subject lacks the tag; leaves a
  subject that already starts with `[<stem>]`; only the subject line changes
  (body preserved); a subject that starts with a DIFFERENT `[other]` tag —
  decide (prepend `[<stem>] ` so both are present, or treat as user intent?
  lean: prepend, so the plan tag is always present).
- An in-process finish-run test (reuse the new `finish_integration.rs`
  harness): `clank finish <plan> -m "custom subject" -m "why"` lands a finish
  commit whose subject starts with `[<stem>]`, and `clank status` reports NO
  `fix_commit_tag`.

## Acceptance criteria

- `clank finish <plan> -m "<no tag>"` produces a finish commit tagged
  `[<stem>]` — no `fix_commit_tag`.
- An already-tagged `-m "[<stem>] ..."` is not double-tagged.
- Validation (WHAT/WHY) still enforced; body untouched. clippy at baseline.

## Deploy

`cargo install --path crates/cli --force` (no skill change required, though
the skill could mention that the tag is added automatically).
