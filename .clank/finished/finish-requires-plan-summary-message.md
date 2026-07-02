# finish-requires-plan-summary-message

Make `clank finish` require a real, whole-plan commit message — written as
if the entire plan were a single commit (WHAT briefly, WHY especially) —
instead of defaulting to the useless `[<stem>] finish`. This message becomes
the canonical one-line-plus-body summary of the plan, so a later squash can
simply adopt it (follow-up plan) rather than inventing a message.

## Why

A plan is implemented as N commits; its natural "one commit" summary should
be authored once, by the master agent that has the full OUTCOME in context
(what actually got built, which often diverges from the plan doc as scope is
cut or the approach changes under review). Today `clank finish` defaults to
`[<stem>] finish` — a placeholder that describes nothing. Making the finish
message a proper whole-plan commit message means:
- the finish commit is a meaningful history entry on its own, and
- squashing a plan to one commit can adopt it verbatim — no separate
  `--squash "<MSG>"`, no message invented at squash time (the reason batch
  squash has been blocked).

We considered instead requiring the plan doc's first paragraph to read like a
commit message, but the finish message is more accurate (outcome vs. intent)
and is the thing we can actually make mandatory at the moment it's authored.

## Scope (this plan)

Just the finish-message requirement + the skill guidance. Adopting the
message at squash time (`finish --squash` reading it; a batch `clank squash
[<base>]`) is a deliberate FOLLOW-UP — this plan is its prerequisite.

## Implementation

- **`crates/cli/src/cli/finish.rs`**: add a pure
  `validate_finish_message(msg: &str, stem: &str) -> Result<(), String>` and
  call it wherever a finish commit message is authored — the finalize path
  (`finalize`, currently `message.unwrap_or("[{stem}] finish")`) and the
  `--amend` path (`amend_already_finished`). On the finalize-CREATING path,
  a message is now REQUIRED (no silent `[<stem>] finish` default).
  - Reject when: absent/empty; a bare placeholder subject (case-insensitive,
    with a leading `[<stem>]` and trailing dots stripped: `finish`, `finished`,
    `done`, `wip`, `complete`) — which also catches the `[<stem>] finish`
    default; or — the PRIMARY check (ruthless 3018ae6) — a subject-only
    message with NO WHY body. Key on the body, not a subject-length floor: a
    length floor false-rejects a concise subject with a real WHY, and worse
    false-ACCEPTS a long subject with no WHY (silently, since the error only
    fires on reject). Keep a tiny body-length floor only as a secondary guard
    against a trivially-empty body. The ERROR does the real teaching.
  - The rejection error must be educational, not just "too short": explain
    that the finish message is the whole plan's commit message — state the
    WHAT briefly and especially the WHY — and show the shape
    (`clank finish <plan> -m "<subject>\n\n<why + effects>"`).
  - Keep the internal `finalize()` fn signature (tests call it directly with
    `None`); enforce the requirement in `run()` before dispatching to
    finalize, so the CLI is strict while the mechanics stay unit-testable.
  - **Validate the message that LANDS, not just `-m`** (codex c2122b3):
    `apply_squash` collapses the range — including the finalize commit — into
    ONE commit carrying the `--squash` MSG, so with `--squash` it's that MSG
    that must be validated (validating `-m` there would let a placeholder
    squash message land). Model this in a pure `message_requiring_validation`
    (squash MSG → validate it; else the finalize/amend message) so the routing
    is unit-testable alongside `validate_finish_message`. `--purge` is NOT
    exempt (codex 53a9edb): it still authors a finalize commit first, which
    SURVIVES if the strip rewrite is refused, so its message must be validated
    too — the transient commit must never be the `[stem] finish` placeholder.
  - **Repeatable `-m`** (codex c2122b3): make `FinishArgs.message` a
    `Vec<String>` composed git-style (paragraphs joined by a blank line) via
    `compose_finish_message`, so the educational error's `-m "<subject>" -m
    "<why>"` form is actually accepted and naturally yields subject+body.
  - **Stamp the transient finalize/amend commit with the LANDING message**
    (codex 053f9d1): `finalize`/`amend` run BEFORE the post-finalize squash,
    which can be refused (protected branch, etc.) after the commit exists —
    leaving `[stem] finish` on HEAD. `finalize_commit_message(squash, -m)` =
    the squash MSG when squashing, so a refused squash leaves the validated
    message, never the placeholder. Regression: an in-process finish-run test
    (`--amend --squash` on protected `main`) asserts HEAD keeps the squash
    subject, not `[foo] finish`.
  - Decision to flag (below): hard reject vs. warn, and whether to add an
    escape hatch.
- **`crates/cli/src/cli/mod.rs`**: update the `FinishArgs.message` doc
  (`-m`) from "Override the default `[<stem>] finish` commit message" to
  "REQUIRED: the whole plan's commit message (WHAT briefly, especially WHY)."
- **`crates/cli/src/cli/setup_assets/skill_master.md`**: the finalize
  guidance (lines ~14, ~48, ~59 say `clank finish <plan>`) becomes
  `clank finish <plan> -m "<whole-plan commit message>"`, with a sentence:
  write it as if the whole plan were one commit — the WHAT in one line, the
  WHY in the body — because it IS the plan's squash message; never "finish".
  (`clank setup` reinstalls the skill; note in Deploy.)

## Rewrite a finished plan's message with a bare `-m`

Since the finish message now matters (it's the squash summary), make fixing
it ergonomic: `clank finish <plan> -m "<better message>"` on an ALREADY-
finished plan just rewrites the finalize commit's message — no `--amend`
ceremony. Routes to the existing `amend_already_finished` (re-commit HEAD
with the new message, finalize tree untouched), gated on
`require_head_is_finalize` (HEAD must be the plan's finalize commit; clear
error if work is stacked on top). Fires only for a bare `-m` (no `--amend`,
no `--purge`/`--squash` — those keep their existing paths). The new message
runs through the same `validate_finish_message`, so you can't rewrite it back
to a placeholder.

## Decision to flag for review

**Hard reject vs. warn.** The user leans reject ("reject short messages like
'finish' … or at least warn"). Proposal: hard reject on the finalize-creating
path (the agent is right there and can fix it), with the educational error.
Open question for reviewers: an escape hatch (e.g. `--allow-terse`) for the
rare legitimate short message, or keep it strict with no override? Lean: no
override in v1 — the whole point is to force the summary; add one only if a
real need shows up.

## Tests

- `validate_finish_message`: rejects `None`/empty, `finish`, `[<stem>] finish`,
  `done`/`wip`; rejects a subject-only message EVEN when the subject is long
  (the false-accept guard); accepts a concise subject + real WHY body.
- The finalize path via `run()` (in-process, no binary spawn) bails with the
  educational error when `-m` is absent or a placeholder, and succeeds with a
  good message (finish commit lands with that message).
- `--amend` re-commit enforces the same bar.
- The bare-`-m`-on-finished path rewrites the finalize commit's message (and
  still validates it).
- Existing finalize unit tests that call `finalize(..., None)` keep passing
  (the requirement lives in `run()`, not `finalize()`).

## Acceptance criteria

- `clank finish <plan>` with no `-m` (or a placeholder/too-short `-m`) is
  rejected with a message teaching WHAT-briefly + WHY-especially; the default
  `[<stem>] finish` message can no longer land via the finalize path.
- A good `-m` finalizes as today, with that message on the finish commit.
- `clank finish <plan> -m "<better>"` on an already-finished plan rewrites the
  finalize commit's message (validated), with no `--amend` needed.
- skill_master.md instructs the whole-plan-commit-message convention.
- clippy at baseline; tests pass.

## Deploy

`cargo install --path crates/cli --force`, then `clank setup` to reinstall
the updated skill into `~/.claude` / `~/.codex`.

## Follow-up (not this plan)

`finish --squash` adopts the finish commit's message (drop the separate
`--squash "<MSG>"`); then a batch `clank squash [<base>]` walks each plan's
finish commit and adopts its message — zero message input, because they were
all authored at finish time.
