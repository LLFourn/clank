# config-command

## Summary

Make `clank config` a useful command that shows all config
keys with their current values and lets agents set them.

## Design

### `clank config` (no args)

Prints all config keys grouped by surface, with current
effective values and defaults:

```
review:
  force_review_on_misc_commits = true  (default: true)
  force_review_on_plan_commits = true  (default: true)
  ad_hoc_reviewers = null              (default: null)
  require_commit_prefix = false        (default: false)
```

Source: two-layer merge of `~/.clank/config.json` and
`<repo>/.clank/config.json`.

### `clank config set <key> <value>`

Sets a key in `<repo>/.clank/config.json`. Dot-separated
keys: `review.force_review_on_misc_commits false`.

Validates the key exists and the value is the right type
before writing. Prints the new effective value after write.

Does NOT commit — the operator decides when to commit
config changes (they might set several keys).

### `clank config get <key>`

Prints just the effective value for one key. Useful for
scripts.

### `clank config --json`

Dumps the full effective config as JSON.

## Implementation

- `cli/config.rs`: add `pub async fn run(args: ConfigArgs)`,
  `ConfigArgs` with subcommands `Get`/`Set` + bare dump.
- `cli/mod.rs`: `ConfigArgs`, `ConfigCmd` enum.
- `main.rs`: wire `Config` variant.
- Config write: read existing repo config.json, deep-merge
  the new key, write back. Use `serde_json::Value` for the
  merge so we don't clobber unknown keys.

## Tests

- Bare `clank config` prints all keys with values.
- `clank config set review.force_review_on_misc_commits false`
  writes to repo config.json.
- `clank config get review.force_review_on_misc_commits`
  returns the effective value.
- `clank config --json` returns valid JSON.
- Unknown key errors cleanly.
- Invalid value type errors cleanly.
