# clank-config-local-only

## Problem

`.clank/config.json` is repo-local operating state and must not be tracked. It holds the local roster and repo-scoped config, so `clank init` may create or update it, but the file belongs under the managed `.clank/.gitignore` allow-list's catch-all ignore.

The current tree has contradictory signals:

- `.clank/.gitignore` correctly ignores `.clank/config.json` through the `/*` catch-all because only explicit allow-list carve-outs are tracked.
- `README.md` says only `plans/` and `finished/` are committed.
- `teams_config.rs` says repo config is gitignored/per-user-per-repo.
- Some init/status comments and probes still describe `config.json` or `agents/` as needing to be tracked.
- `clank init`'s `TRACKED_PROBES` includes local-only paths, which can produce false warnings when an ancestor ignore rule matches `.clank/config.json` or `.clank/agents`.
- Some TUI comments and test strings say changing the roster touches committed/tracked config, which is wrong if `.clank/config.json` is local-only.

This ambiguity keeps causing design drift around whether roster changes are shared by Git.

## Goal

Make the local-only invariant explicit, tested, and internally consistent:

`.clank/config.json` is created by `clank init` when needed, read by workflow commands, and ignored by Git. It is not part of Clank's tracked plan history.

## Scope

1. Treat `crates/cli/src/init_facts.rs::CLANK_GITIGNORE_ENTRIES` as the source of truth: the only `.clank/.gitignore` carve-outs are `plans/`, `finished/`, and `.gitignore`; `.clank/config.json` stays ignored by absence from that allow-list.
2. Clean stale comments/docs that imply `.clank/config.json`, `.clank/agents/`, feedback, or cache are tracked.
3. Fix `clank init`'s model, not just its comments: remove `.clank/config.json` and `.clank/agents` from `TRACKED_PROBES`, leaving only tracked plan artifacts (`.clank/plans` and `.clank/finished`) so ancestor ignore warnings are not emitted for local-only paths.
4. Update TUI/add-agent comments, tests, and any user-facing confirm modal text for roster mutations so they describe local `.clank/config.json` writes, not tracked/committed working-tree dirt. If the modal has no such displayed warning, make that explicit in the code comment or test update so the old rationale cannot survive.
5. Add or tighten tests proving:
   - `clank init`'s managed `.clank/.gitignore` has exactly the intended tracked carve-outs (`plans/`, `finished/`, `.gitignore`) and no carve-out for `.clank/config.json`.
   - tracked Clank surface is limited to plan artifacts (`plans/`, `finished/`) plus the managed ignore file where relevant.
   - local-only paths such as `.clank/config.json`, `.clank/agents/**`, `.clank/cache/**`, drafts/queue/html/worktrees are ignored by the allow-list catch-all.
   - ancestor/global ignore warnings are not checked for local-only paths, because `TRACKED_PROBES` contains tracked paths only.

## Non-Goals

- Do not redesign the roster model.
- Do not move roster data to user-global config.
- Do not make `.clank/config.json` shared or force-addable.
- Do not change plan/finished tracking semantics.

## Validation

- Run the focused init/gitignore tests.
- Run status/TUI tests touched by comment or behavior changes.
- Confirm `git check-ignore -v .clank/config.json` is ignored by `.clank/.gitignore`.
