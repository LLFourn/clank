# pr-review-submit-outcome

`clank pr-review submit` always publishes the review as a plain
COMMENT — `submit_argv` hardcodes `-f event=COMMENT`
(pr_review.rs). But a published GitHub review usually wants a real
outcome: APPROVE or REQUEST_CHANGES. Redesign submit to MANDATE the
outcome via a flag, using GitHub's canonical review events.

## Change

- `PrReviewSubmitArgs` gains a REQUIRED `--event` (no default — force
  the master to state the outcome), value enum mapping to GitHub's
  three review events:
  - `approve`         → `APPROVE`
  - `request-changes` → `REQUEST_CHANGES`
  - `comment`         → `COMMENT`
- `gh::submit_argv` takes the event instead of hardcoding COMMENT:
  `-f event=<APPROVE|REQUEST_CHANGES|COMMENT>`.
- Thread it through `submit_with` / `gh::submit_review`.

## Body rules (GitHub's, enforce them)

GitHub requires a non-empty body for `REQUEST_CHANGES` and `COMMENT`,
but allows an empty body for `APPROVE`. Today `extract_submit_body`
unconditionally refuses an empty/placeholder body. Adjust:
- APPROVE: body OPTIONAL (skip the `-f body=` arg, or send empty) —
  don't force a master.md summary just to approve.
- REQUEST_CHANGES / COMMENT: body REQUIRED (keep the current
  empty/placeholder refusal).

## Keep the existing guards

- Convergence gate stays: submit still requires the local review to
  be converged (compute_gate == Finished) before publishing.
- The pre-submit reviewer-reply re-sweep stays (only master's
  top-level comments publish).
- resolve-on-demand of the pending review id is unchanged.

## Naming note

Use GitHub's vocabulary (`approve` / `request-changes` / `comment`),
NOT clank's `VerdictArg` (which has `finished`, a clank-only concept
with no GitHub review-event equivalent). This is the GitHub
publish boundary, so it should speak GitHub.

## Skill update

`setup_assets/pr_review_skill.md`: `clank pr-review submit` now takes
`--event <approve|request-changes|comment>`; document the body rule
(approve may omit it). Bump the content-guard test's pinned verb
surface if it asserts the submit form.

## Testing

In-process (no clank-binary spawning):
- `submit_argv` emits the right `event=` for each variant.
- APPROVE with empty body omits/permits the body; REQUEST_CHANGES /
  COMMENT with empty body is refused (extract_submit_body path).
- `--event` is required (clap parse error when omitted).
- submit still refuses when not converged (existing guard intact).

## Non-goals

- Reviewer-side verdicts (`clank pr-review note`) — unchanged; this
  is only the master's PUBLISH outcome.
- Dismissing/editing already-published reviews.
