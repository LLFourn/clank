# remove-research-team-seed

## Problem

`clank setup` currently seeds a hardcoded `research` team via
`seed_research_team`. That makes a personal/team composition appear as a
magic setup side effect instead of emerging from the normal user config
surfaces:

- define reusable agents with `clank agent add --global --tool ...`
- compose saved teams with the team config/model
- seed a repo explicitly with `clank init --team <name>`

This also caused confusion because `research.grok` was written as an inline
team member rather than as a reusable `agents.grok` library entry, so Grok could
exist in `clank team list` while still being absent from by-name add-agent
flows.

## Goal

Remove the hardcoded `research` team setup seed. `clank setup` should install
skills/hooks and deliberate global defaults only; it should not create personal
team templates.

## Scope

1. Remove `seed_research_team` and its call from `clank setup`.
2. Remove or rewrite tests that only exist to pin `research` auto-seeding.
3. Audit setup-time user-config mutations and leave only broadly intentional
   defaults. In the current tree, `seed_autosquash_default` is the other
   production setup seed; keep it only if the code and tests make clear that it
   is an explicit product default, not a personal/team template.
4. Make sure no docs, setup output, or tests imply that `research` is installed
   automatically.

## Non-Goals

- Do not remove Grok as a supported tool.
- Do not remove the global team-template system.
- Do not change `clank init --team` resolution semantics.
- Do not modify the user's existing `~/.clank/config.json`; this plan changes
  future setup behavior only.

## Validation

- Run the focused setup tests.
- Run a source search confirming `seed_research_team` and automatic
  `research` setup references are gone.
- Confirm `clank setup --dry-run` no longer reports seeding `research`.
