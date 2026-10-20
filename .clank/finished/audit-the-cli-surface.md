# audit-the-cli-surface
# Audit the CLI surface: find what is hard to understand

## Goal

Produce a findings document, `docs/cli-audit.md`, that answers three questions
about every clank command and subcommand:

1. **Documented?** Is it in the README command reference (and the skills, where
   agents use it), and does its `--help` say what it is for in terms a user
   understands?
2. **Should it exist?** Is it a command a person or agent actually reaches
   for, or plumbing, legacy, a no-op, or a duplicate of another command whose
   difficulty to document is the symptom?
3. **Is it where you'd look?** Do the subcommand groups follow one organising
   rule, with consistent verbs and no two commands that sound alike but do
   different things?

This plan writes only the audit. Each change it recommends goes into its own
queued draft plan. This plan renames, removes and regroups nothing.

## Method

- Build the inventory from clap, not from memory. Add a unit test in
  `cli::command` that walks `Cli::command()` and emits every command path,
  including hidden ones, with its about text and flags. Produce the table from
  that walk.
- Cross-check each path against the README `## Command reference` and
  `setup_assets/skill_*.md`. Mark each one documented, partly documented, or
  missing.
- Classify each command's audience: human, agent, hook/plumbing (installed by
  `setup`, never typed), or legacy/migration.
- For each command, record the one-sentence answer to "when would I run this?".
  A command that has no such answer, or needs a paragraph of caveats in its
  `--help`, is a candidate for removal or merging.
- Group the findings by cause, not by command. Example causes: plumbing
  exposed as user commands, one concept split across groups, a no-op flag,
  help text that describes the implementation.

## Already visible from a first look (verify, don't assume)

- README table omits: `remote`, `monitor`, `nudge`, `attending`, `run`,
  `rereview`, `unblock`'s relation to `block create`, `agent swap`,
  `agent launch`, `events reply` / `events read`, `queue unqueue`.
- `promote` means two things: `queue promote` (activate a plan) and
  `agent promote` (make master). The `--help` for `agent promote` has to
  apologise for the clash.
- `block` is described as "ask the human" but is a group
  (`create`/`clean`/`migrate`). `unblock` is a separate top-level command.
  `block migrate` is a one-off legacy migration.
- `auto on|off` take a `--role` that the skills call a no-op.
- `export` (dump the roster as JSON) and `team save`/`team show` overlap.
- Plumbing sits at top level next to user commands: `stop-hook`, `monitor`,
  `rewire` (a git hook), `wait` (agents must never run it themselves).
- Lifecycle verbs are scattered across top level: `finish`/`unfinish`,
  `stash`, `pick`, `purge`, `rereview`, `fork`. Check whether a `plan` group,
  or a written rule for what is top-level, would read better.
- Several `--help` texts carry implementation detail or plan names. One
  example: `agent start` cites "(agent-start-bootstraps-missing-skeleton)".
- `queue remove` vs `queue unqueue`, and `events ack` vs `events read`: check
  that each pair's difference is obvious from the names.

## Deliverable

`docs/cli-audit.md` contains:

- the full inventory table: path, audience, documented status, verdict
  (keep / fix doc / rename / merge / hide / remove);
- the findings, grouped by cause, each citing the commands it covers;
- a proposed organising rule for the top level and its groups, with the
  resulting tree;
- a list of follow-up draft plans, each queued via `.clank/drafts/` +
  `clank queue add`, ordered by how much confusion each one removes.

## Acceptance

- The inventory test exists, walks the real `Cli`, and the table in the doc
  matches its output; no hand-built command list.
- Every command path from the walk appears in the doc with a verdict.
- Each finding names its cause and the commands it covers.
- The follow-up drafts are queued. Nothing user-facing changes in this plan.

## As built

- `docs/cli-audit.md`: 101 command paths, each with when you would run it,
  audience, documentation status, verdict and finding; findings F1-F8 by
  cause; a proposed organising rule and tree.
- `cli::command::tests`: the inventory is built from `Cli::command()`
  (`rows()`), embedded in the doc and checked by
  `the_cli_audit_inventory_is_the_real_tree` (`CLANK_BLESS_AUDIT=1`
  regenerates it); `the_cli_audit_judges_every_command_path` requires
  exactly one verdict per path; `the_cli_audit_inventory_records_every_argument`
  checks no declared argument is dropped.
- Seven follow-ups queued at 510-560, including `cli-help-is-beautiful` (515)
  for the user's request that the CLI be beautiful.
- Review corrections: `nudge` keeps an attempt history; `export` stays
  (raw shape, unlike `agent list -j`); the hidden `shelve` aliases stay,
  since clank has no release boundary to expire them against.
