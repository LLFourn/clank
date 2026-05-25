# config-command

## Summary

Make `clank config` the single interface for all clank
configuration, including hooks. Everything lives in
`config.json` (user-level `~/.clank/config.json`, repo-level
`<repo>/.clank/config.json`). Hooks move from separate
`hooks.json` files into the same layered config.

## Design

### `clank config` (no args)

Prints all config keys grouped by section, with current
effective values, defaults, source, and description:

```
review:
  adhoc_feedback = true          (default: true, source: default)
    Require review for ad-hoc (non-plan) commits. bool.
  plan_feedback = true           (default: true, source: default)
    Require review for plan-attributed commits. bool.
  require_commit_prefix = false  (default: false, source: default)
    Require [plan] or [misc] commit title prefixes. bool.

hooks:
  master_work = "say work"       (default: null, source: repo)
    Shell command to run when master has new work. string or null.
  reviewer_work = null            (default: null, source: default)
    Shell command to run when a reviewer has work. string or null.
  plan_finalized = null           (default: null, source: default)
    Shell command to run when a plan is finished. string or null.
  idle = null                     (default: null, source: default)
    Shell command to run on idle (no work). string or null.
```

Each key's description is compiled into the binary — the
catalog is a static `&[KeyDef]` with name, section, type,
default, and one-line help.

### `clank config <key> get`

Prints the effective value for one key. Key is dot-separated:
`review.adhoc_feedback`, `hooks.master_work`.

### `clank config <key> set <value>`

Sets a key in `<repo>/.clank/config.json`. Validates the
key exists and the value is the right type before writing.
Prints the new effective value after write.

Does NOT commit — the operator decides when to commit
config changes (they might set several keys).

### `clank config --json`

Dumps the full effective config as JSON.

## Hooks migration

Move hooks from `~/.clank/hooks.json` and
`<repo>/.clank/hooks.json` into `config.json` under a
`hooks` key:

```json
{
  "review": { ... },
  "hooks": {
    "master_work": "say plan work",
    "idle": "say idle"
  }
}
```

The loader reads `config.json` first. If `hooks.json` exists
and `config.json` has no `hooks` section, the loader falls
back to `hooks.json` and emits a deprecation warning via
`tracing::warn`. Once the user runs
`clank config hooks.<key> set <value>`, the value lands in
`config.json` and the `hooks.json` fallback is superseded.

Key names use underscores (`master_work`) in config.json.
Serde aliases accept the kebab-case form (`master-work`)
used in the old `hooks.json` files.

## Rename (already done)

`force_review_on_misc_commits` → `adhoc_feedback`,
`force_review_on_plan_commits` → `plan_feedback`.
Old JSON keys accepted via `#[serde(alias)]`.

## Implementation

- `cli/config.rs`: add `hooks: HookConfig` to `Config` and
  `ConfigFile`. Load hooks from `config.json` `hooks` section.
  Add `pub async fn run` with bare dump, get, set.
- `hook_config.rs`: simplify to read from `Config` instead
  of separate `hooks.json` files.
- `cli/mod.rs`: `ConfigArgs` with positional key + action.
- `main.rs`: wire `Config` variant.
- Config write: read existing repo config.json, deep-merge
  the new key, write back.

## Tests

- Bare `clank config` prints all keys with values.
- `clank config review.adhoc_feedback get` returns value.
- `clank config review.adhoc_feedback set false` writes.
- `clank config hooks.master_work set "say hello"` writes.
- `clank config --json` returns valid JSON.
- Unknown key errors cleanly.
- Invalid value type errors cleanly.
- Hooks loaded from config.json work in wfw lifecycle.
