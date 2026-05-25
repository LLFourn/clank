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
effective values and defaults:

```
review:
  adhoc_feedback = true          (default: true)
  plan_feedback = true           (default: true)
  require_commit_prefix = false  (default: false)

hooks:
  master_work = null             (default: null)
  reviewer_work = null           (default: null)
  plan_finalized = null          (default: null)
  idle = null                    (default: null)
```

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

The loader reads from `config.json` only. Old `hooks.json`
files are ignored (can be cleaned up manually). The key
names match `HookEvent::as_str()` but with underscores
(serde rename).

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
