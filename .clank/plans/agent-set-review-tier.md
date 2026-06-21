# agent-set-review-tier

Add a `clank agent` subcommand to change a reviewer's tier (commit ↔ gate)
in place, without the remove+re-add dance (which today tears down and
respawns the agent's session/zellij pane).

## Problem

A roster agent's tier (commit vs gate reviewer) can only be changed by
`clank agent remove <name>` + `clank agent add <name> --review <tier>`.
That round-trip drops the agent's bound session and (in zellij) closes
and respawns its pane. There's no in-place tier edit, so people hand-edit
`.clank/config.json` instead.

## Design

- New subcommand: **`clank agent set-review <name> <commit|gate>`** (name
  open — `set-review` mirrors the existing `--review` flag; reviewers can
  pick `review` / `set-tier` if preferred).
- Pure config mutation: read the repo roster (via the fail-closed loader
  `read_repo_config`), set `agents[<name>].role` to `Commit`/`Gate`,
  write back. No session/pane disturbance (the pane title is
  `"<name> (reviewer)"` regardless of tier, so nothing visual changes).
- Errors (mirror `set_repo_master`'s shape):
  - agent not on the roster → the standard not-on-roster error;
  - agent IS the master → refuse ("master isn't a reviewer tier; use
    `clank agent <promote/demote>`") rather than silently demoting;
  - already at that tier → friendly no-op note.
- Reuse the existing `ReviewKindArg` (commit|gate) clap enum and
  `RosterRole` mapping.

## Integration points

- `crates/cli/src/cli/mod.rs`: add `AgentCmd::SetReview(AgentSetReviewArgs)`
  next to `Add`/`SetMaster`/`Remove`; args = `name: String`,
  `review: ReviewKindArg`, `repo: Option<PathBuf>`.
- `crates/cli/src/cli/agent.rs`: dispatch + a `set_review` thin shell +
  `set_repo_review(repo, label, RosterRole)` core (modeled on
  `set_repo_master`/`set_repo_review` at ~702–742).
- Document it in the `clank-master` skill (`setup_assets/skill_master.md`
  roster-commands list) so master knows the command exists.

## Testing (no-binary-spawning)

- `set_repo_review` flips commit→gate and gate→commit on a roster fixture;
- refuses on the master label and on an unknown label (fail-closed);
- in-place: the agent's other fields (tool/launch/session-irrelevant)
  are preserved.
- CLI parse test (`agent set-review codex gate` parses).

## Acceptance

- `clank agent set-review <name> commit|gate` changes the tier in place,
  no remove/re-add, no session or pane churn.
- Master label and unknown label are rejected with clear messages.
- Existing tests green; clippy within budget (cli ≤30); fmt clean.
