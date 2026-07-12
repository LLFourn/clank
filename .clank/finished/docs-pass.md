# docs-pass
# docs pass: bring README (and doc surface) back in sync with the binary

The README predates most of the current surface and now actively
misleads: its `feedback write` example uses a `--plan` flag and a
stdin body (the real surface is a required `-m`, no `--plan`, and a
leading verdict header is STRIPPED not validated); the layout section
shows the pre-split `~/.claude/skills/clank/SKILL.md` (today it's
`clank-master` / `clank-reviewer` / `clank-pr-review`); the Stop-hook
section describes both tools long-polling in-hook (claude now never
waits in-hook — it arms a background `clank wait`; only codex
long-polls; grok is passive); and of ~28 subcommands it documents
about seven — no queue/drafts flow, no team templates or `fork`, no
`pr-review`, no `wait --for` observer mode, no extra wait events /
controller repos, no `open`/console/zellij story, no `log`/`html`, no
grok anywhere.

## Ground rule

Every command, flag, path, and behavioral claim in the rewritten docs
is VERIFIED against the current binary — `--help` output at minimum,
a scratch-repo run for the worked examples. Nothing is written from
memory. The skills (`setup_assets/`) are the operational teaching and
were maintained plan-by-plan; the README is orientation — it should
introduce the model and index the surface, not duplicate the skills.

## Milestones

- **M1 audit**: sweep README.md + RELEASE-CHECKLIST.md against
  `clank --help` and each subcommand's help; produce the stale-claim
  list in the commit message (drives M2's scope honestly).
- **M2 README rewrite**, keeping it an orientation doc:
  - Install/setup: tools now include grok where true; what `setup`
    actually writes (role-split skills + pr-review skill + slash
    command + hook entries).
  - The core loop as actually dogfooded: drafts dir → `queue add` →
    promote item → intro review (CONTINUE/FINISHED/REQUEST_CHANGES,
    correct `feedback write -m` shape) → implement in reviewable
    commits → `finish -m`.
  - Roster & teams: `agent add/promote/remove/list/start`, ALL FOUR
    review tiers with their meanings — commit, plan, final, and gate
    (gate folds into both plan- and final-stage review) — plus
    `team save/list/show/delete`, `init --team`, `fork`.
  - The wait surface: `wait`, `--peek`, `--for commit|finished|stopped`
    observer mode, and extra wake sources (`wait_events` config +
    `--event`, the controller-repo pattern, own-action default) —
    a short section pointing at the master skill for depth.
  - Stop hook per-tool models (claude arm-the-wait, codex in-hook
    poll + `wait_timeout`, grok passive skill-taught loop).
  - Workspace: `open` (console / zellij tab), `status` + `status
    --tui`, `log`, `html`.
  - Plan surgery: `stash` (incl. `--to-queue`), `pick`, `purge`,
    `unfinish`, `diff`, blocks (`block` / `unblock`).
  - Layout section: current on-disk truth (role-split skills,
    local-only `.clank/config.json`, queue/drafts, cache) and the
    doctor transcript refreshed from a real run.
- **M3 RELEASE-CHECKLIST.md**: verify each step still matches
  reality; fix or delete stale steps.

## Acceptance

- No STALE CLAIM survives in the audited docs — README.md,
  RELEASE-CHECKLIST.md, and `setup_assets/` where applicable; source
  code, tests, and finished-plan history are out of scope. Stale
  claims are exact forms, not bare tokens (several tokens are live
  syntax elsewhere — `--from` is current on `clank pick`,
  `--from-stdin` on `clank rewire`, and "long-poll" correctly
  describes codex's in-hook wait; codex 2c2b2e8):
  - the single `~/.claude/skills/clank/SKILL.md` layout (skills are
    role-split: clank-master / clank-reviewer / clank-pr-review);
  - `feedback write` taking a `--plan` flag or a stdin body (real
    surface: required `-m`, and a leading verdict header is stripped);
  - the claim that CLAUDE's hook long-polls in-hook (claude arms a
    background wait; only codex long-polls in-hook);
  - `clank wfw` as a command name;
  - `--from` on QUEUE ADD specifically (drafts dir + `queue add`
    replaced it);
  - "two/three skill files" counts that no longer match what setup
    writes.
- M1 produces a concrete subcommand INVENTORY from `clank --help`
  plus each nested help; the rewrite is checked against that
  inventory line-by-line (every entry indexed in README — one line
  each is fine; depth stays in the skills). No vague counts.
- Worked examples in README were executed against a scratch repo (or
  are verbatim `--help` output); the doctor transcript is from a real
  run of the current binary.
- All four tiers (commit / plan / final / gate) are documented with
  their actual semantics.
- Doc-only where possible; if a help string itself turns out wrong,
  fixing it is in scope (with tests green, fmt/clippy at the 18/6
  baseline).
