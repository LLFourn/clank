# bare-init-uses-the-default-team
# Bare `clank init` on a repo with no roster seeds it from the user's `default` team, when one exists

## Why

The user, 2026-10-17: "Like clank init, when there is no team specified, should use the default team."

Today bare `clank init` (no `--team`) on a repo without a valid roster always writes an empty roster (`init::bootstrap_empty_repo_config`). The user then has to add every agent by hand, even though their user-scope config already holds a team named `default` (`~/.clank/config.json#/teams/default`, beside `fast-dev`, `research`, `sec`, `solo` and `review3`).

## What to build

- **The default team.** Bare `clank init`, when the repo has no valid roster (the existing `repo_has_team` guard), registers the user-scope team named `default` exactly as `clank init --team default` would (`register_repo_team_guarded`). A team literally named `default` is the convention; no new config key is added.
- **No `default` team** (or no `$HOME`): today's empty roster, unchanged.
- **Unchanged cases:**
  - an existing valid roster is never clobbered on re-init;
  - an explicit `--team <name>` behaves as today.
- **Output.** Init says which team it used ("seeded the roster from your `default` team"), or that it wrote an empty roster because there is no `default` team.
- **Docs.** Update the `--team` help text and the README quickstart to say bare init uses the `default` team.

## Acceptance

Tests use temp homes and repos through the init cores, not the binary.

- With a `default` team in the user config, bare init on a fresh repo produces that team's roster, master included.
- Without one, bare init writes the empty roster.
- Bare re-init on a repo with a roster leaves it untouched, even with a `default` team present.
- `--team <other>` still uses `<other>`.

## As built

- **`init::seed_bare(home, repo)`** is the env-free core for bare `clank init`, called from `init::run` when `--team` is omitted. It checks, in order:
  1. A repo with a valid roster (`repo_has_team`) returns before any user config is read.
  2. If the user config declares a team named `default` (`DEFAULT_TEAM`), it goes through the same `register_repo_team` as `--team default`, and prints "seeded the roster from your `default` team".
  3. Otherwise, `bootstrap_empty_repo_config` writes the empty roster.
- **`user_team_declared`:** an absent user config is "not declared". An unreadable one is an error. A declared-but-broken `default` (dangling reference or malformed member) errors through `register_repo_team`'s existing messages, never as an empty roster.
- **Wording:**
  - `--team` help text, the empty-roster message and the missing-team error now describe the first-add-becomes-master rule instead of a `clank agent promote` step;
  - the empty-roster message also mentions the `default` team;
  - the README notes that a template saved as `default` seeds bare init.
- **Tests** (`cli::init`):
  - a fresh repo with a `default` team gets its roster, master and descriptions included;
  - another team only, an empty home, or no home writes the empty roster;
  - bare re-init keeps the roster, even with an unreadable user config;
  - a broken `default` team, or an unreadable user config, is an error.
- **Mutations, each caught:**
  - looking up the team before the roster guard;
  - never using the default team;
  - treating an absent user config as an error.
