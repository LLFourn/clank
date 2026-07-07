# purge-wfw

`wfw` was renamed to `wait` (finished: rename-wfw-to-wait), which
deliberately kept compat aliases and left comment references. Complete the
rename: purge every live mention of `wfw`. No alias, no invariant name, no
comment citation.

## Scope (live tree)

Full live-tree sweep found exactly these (history excluded — see Keep):

1. **CLI alias** — `crates/cli/src/cli/mod.rs:799`: drop
   `visible_alias = "wfw-timeout"` and the doc-comment sentence that
   explains it (`:797`). `--wfw-timeout` stops parsing; `--wait-timeout`
   is unaffected.

2. **Config alias** — `crates/core/src/agent_config.rs:56`: drop
   `alias = "wfw_timeout"` from the `wait_timeout` serde attr and rewrite
   the field doc (`:51-52`). Delete the test
   `wait_timeout_reads_legacy_wfw_timeout_key` (`:174`) — do NOT replace
   it with a "legacy key is rejected" test; the framework enforces
   absence. Keep/ensure a plain `wait_timeout` read test remains (there
   is already round-trip coverage in `agent_store.rs`).

3. **Invariant rename** — `wfw-output-is-a-minimal-hint` →
   `wait-output-is-a-minimal-hint` everywhere it is cited:
   `crates/cli/src/cli/wait.rs:553,1145`,
   `crates/cli/src/cli/stop_hook.rs:513,543,563,951`,
   `crates/cli/src/cli/setup.rs:753`. (The finished plan file that coined
   it keeps its name; the invariant as cited in living code takes the new
   name.)

4. **Plan-name citations** — `crates/core/src/wait.rs:824,3032` cite
   finished plan `wfw-master-returns-on-dirty-plan-any-gate`. Reword both
   comments to state the behavior ("master gets a work item when the plan
   is dirty regardless of gate state") instead of citing the wfw-named
   plan.

5. **Verify sweep** — `grep -rin wfw crates/ README.md` → empty.

## Config migration BEFORE install

Removing the serde alias silently drops `wfw_timeout` values from
pre-rename on-disk agent configs (an unset `wait_timeout` = indefinite —
a real behavior change, not just a parse error). At install time, before
`cargo install`:

- enumerate every repo's `.clank/agents/*/config.json` with `find`
  (NOT shell globs — `.clank` is hidden), plus any `~/.clank` global
  config, for the `wfw_timeout` key;
- rewrite the key to `wait_timeout` in place;
- validate each migrated file still parses with the NEW binary.

## After install

Re-run `clank setup` so installed skill docs are regenerated — the
currently-installed `~/.claude/skills/clank/SKILL.md` still teaches
`clank wfw` (stale pre-rename output; current `setup_assets/` are already
clean).

## Keep (history, not mentions)

- `.clank/finished/wfw-*.md`, `clank-wfw.md`, `rename-wfw-to-wait.md` and
  all feedback archives — audit trail; renaming them would break
  history/feedback linkage.
- `.clank/worktrees/*` stale checkouts and generated `.clank/html/`
  artifacts — gitignored/regenerated, not live mentions.

## Acceptance

- `grep -rin wfw crates/` returns nothing.
- `--wfw-timeout` no longer parses; no test asserts that it is rejected.
- `wait_timeout` round-trip coverage still exists.
- On-disk configs carrying `wfw_timeout` are migrated before the new
  binary lands.
