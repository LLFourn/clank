# init-auto-claim-master

## Summary

Make `clank init` auto-claim master when no existing agent in the
repo has `role: master`. Today it defaults to reviewers in
non-interactive mode and prompts with a default of No in
interactive mode — the first agent in a repo always ends up as a
reviewer unless the operator explicitly opts in.

## Current behavior

`crates/cli/src/cli/init.rs:248-255`:

```rust
let make_master = if interactive {
    prompt_yes_no(
        "Default this agent to master role (vs reviewers)? [y/N] ",
        false,
    )?
} else {
    false
};
```

In `--yes` (non-interactive) mode: always false.
In interactive mode: default No — operator has to type "y".

## Proposed behavior

Before prompting, scan existing agent configs for any agent with
`role: master`. If one exists, default to reviewers (current
behavior). If none exists, flip the default:

- **Interactive**: prompt with default **Yes** — "No master agent
  in this repo yet. Claim master? [Y/n]"
- **Non-interactive (`--yes`)**: auto-claim master.

If there IS an existing master:

- **Interactive**: prompt with default **No** — "Default this agent
  to master role (vs reviewers)? [y/N]" (current behavior).
- **Non-interactive**: false (current behavior).

## Implementation

In `bootstrap_agent_identity` (`init.rs:248`):

```rust
let has_existing_master = load_all_agent_configs(repo)
    .unwrap_or_default()
    .iter()
    .any(|(_, cfg)| cfg.role == Role::Master);

let make_master = if has_existing_master {
    if interactive {
        prompt_yes_no(
            "Default this agent to master role (vs reviewers)? [y/N] ",
            false,
        )?
    } else {
        false
    }
} else if interactive {
    prompt_yes_no(
        "No master agent in this repo yet. Claim master? [Y/n] ",
        true,
    )?
} else {
    true
};
```

Add `load_all_agent_configs` to the import from `agent_store`.

`load_all_agent_configs` errors are swallowed (`unwrap_or_default`)
so a fresh repo with no `.clank/agents/` directory doesn't fail
init.

## Tests

1. **Non-interactive, no existing agents**: `clank init --yes` in
   a fresh repo → agent config has `role: master`.
2. **Non-interactive, existing master**: pre-seed another agent
   with `role: master`, then `clank init --yes` → new agent gets
   `role: reviewers`.
3. **Existing agent configs dir missing** (fresh repo, no
   `.clank/agents/`): `load_all_agent_configs` returns empty →
   auto-claims master. No error.

Interactive-mode prompt defaults are hard to test in CI without a
PTY; the non-interactive paths cover the logic.

## Acceptance criteria

- First `clank init --yes` in a repo auto-claims master.
- Second `clank init --yes` (different agent) gets reviewers.
- Interactive prompt flips its default based on whether a master
  exists.
- No behavioral change when a master already exists.
