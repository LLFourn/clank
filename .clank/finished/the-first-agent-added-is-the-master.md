# the-first-agent-added-is-the-master
# `clank agent add` with no `--review` makes the new agent master when the roster has no master

## Why

The user, 2026-10-17: "`clank agent add claude` when there is no master should just by default make it the master."

Today `AgentAddArgs::review` has `default_value = "commit"`, so `clank agent add claude` always adds a commit-tier reviewer. On a roster with no master, that leaves a repo nobody drives until a second command, `clank agent promote`, which the user has to know to run.

## What to build

- **The flag.** `--review` becomes optional (`Option<ReviewKindArg>`, no default).
- **The decision.** At repo scope, when `--review` is omitted:
  - if the roster has no master, the new agent is added as master;
  - otherwise it is added at `commit`, as today.
- **An explicit `--review`** is always honoured, master or not.
- **Atomicity.** The "no master" read and the write happen under the same roster write the add path already uses, so two concurrent adds cannot both become master. Follow the repo's existing roster-transition rules (`RosterTransitionPolicy`); an add that becomes master goes through the same path `clank agent promote` would.
- **`--global`** is unchanged: the library has no role.
- **Output.** The command's output, or the roster it prints, says which role the agent got, so a silent master is visible.
- **Docs.** Update the `--review` help text, plus the README and skills wherever they say `commit` is the default.

## Acceptance

Tests use the env-free mutation cores (`add_repo_roster_agent*`), not the binary.

- Adding to an empty roster, or one with reviewers but no master, without `--review`, makes the agent master.
- Adding without `--review` to a roster that has a master adds `commit`.
- An explicit `--review commit` on a master-less roster adds `commit`.
- The by-name path (no `--tool`) follows the same rule.

## As built

- **`AgentAddArgs::review`** is `Option<ReviewKindArg>` with no default. Its help text states the rule.
- **`agent::NewRole`**: `Given(RosterRole)` or `Default`. `NewRole::on(&roster)` resolves `Default` to `Master` when no roster entry is master, and to `Commit` otherwise. A `From<RosterRole>` impl keeps every existing explicit-role caller unchanged: the TUI's add picker and the web add both pin their explicit tier as `RosterRole`.
- **Both add cores** (`add_repo_roster_agent`, `add_repo_roster_agent_by_name`) take `impl Into<NewRole>`. Each holds `roster_lock` (`.clank/roster.lock`, `agent_store::exclusive_lock`) across its read, role decision and `write_repo_transition`. That is one write with the role chosen in it, never an add followed by a promote, and the transition policy is unchanged (`PreserveGate`).
- **Output** names the role and, when the default chose master, adds "(the roster had no master)".
- **README quickstart:** `clank agent add alice --tool claude` with the comment "the first agent is the master", then `clank agent add bob --tool codex`. The `promote` and `--review commit` steps are gone.
- **Tests** (`cli::agent`):
  - a default add to an empty roster, and to one with only a reviewer, makes master;
  - a default add beside a master is `commit`;
  - an explicit `commit` on an empty roster stays `commit`;
  - the by-name path follows the same rule;
  - ten rounds of two concurrent default adds keep both agents with exactly one master.
- **Mutations, each caught:** dropping the roster lock in the inline add (the concurrent test fails); a default that always yields master.
