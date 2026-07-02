# finish-autosquash-config

Add a `finish.autosquash` bool config (global + repo, repo shadows global).
When true, `clank finish <plan> -m "..."` collapses the whole plan into one
commit — using the (now mandatory, validated, auto-tagged) finish message as
the squash message. No `--squash` flag needed per finish.

## Why / how it fits

The finish-message arc already built the pieces: `finish` requires a real
whole-plan `-m` (WHAT + WHY), validates whichever message lands, and
`ensure_plan_tag`s it. `autosquash` just says "always take the `--squash`
path, using that `-m` as the squash MSG" — so a plan that was N commits lands
as ONE, self-documented by its finish message. It's the config-driven form of
the batch-squash idea, applied at the natural point (each finish).

The read-side layering the user asked for (repo shadows global) ALREADY
exists: `config::load_with_home` applies `~/.clank/config.json` then
`<repo>/.clank/config.json`. So resolution is free; the work is (1) adding the
key to the schema and (2) consulting it in `finish`.

## Config plumbing (new `finish` section, `finish.autosquash: bool`, default false)

All in `crates/cli/src/cli/config.rs` unless noted — mirror the existing
`diff`/`review` sections exactly:
- `Config` gains `finish: FinishConfig { autosquash: bool }` (+ `Default`).
- `ConfigFile` gains `finish: Option<FinishFile { autosquash: Option<bool> }>`;
  `apply_layer` reads it and records `present("finish.autosquash")`.
- `KEY_CATALOG` entry `finish.autosquash` (bool, default "false", help).
- `key_to_json_path("finish.autosquash") -> ["finish","autosquash"]`.
- `get_value` arm for `finish.autosquash`.
- `RepoConfigFile` gains `finish: Option<FinishSection>` (round-trip) + the
  `ConfigJson`/`FinishJson` wire shape for `clank config --json`.
- `mod.rs`: `ConfigKey::FinishAutosquash(ConfigKeyArgs)` variant + its arms in
  `key_name` / `key_args` (and the clap subcommand).

Result: `clank config finish.autosquash set true` (writes repo config, as
today), and `~/.clank/config.json` `{"finish":{"autosquash":true}}` for the
global default — repo shadows it via `apply_layer`.

## Setting it GLOBALLY

`clank config <key> set` currently only writes the REPO config
(`set_repo_key`). To set autosquash globally (the main use case — turn it on
everywhere, override per-repo), add a `--global` flag to `clank config` that
writes `~/.clank/config.json` instead of `<repo>/.clank/config.json`
(generalize `set_repo_key` to `set_key(target_path, ...)`). Decision to flag:
include `--global` in this plan (recommended — the user explicitly wants to
set it globally) vs. document hand-editing `~/.clank/config.json` for v1.

## Finish behavior (`crates/cli/src/cli/finish.rs`)

Load the config and, when autosquash is on, route through the existing squash
path — minimal wiring so validation/tagging/rewrite all just work:

```rust
let cfg = crate::cli::config::load(&repo);
let message = compose_finish_message(&args.message);
// autosquash: treat a plain finish as `--squash <the -m message>`. Explicit
// --squash wins; --purge is respected (not auto-squashed).
let mut args = args;
if cfg.finish.autosquash && args.squash.is_none() && !args.purge {
    args.squash = message.clone();
}
```

Because `args.squash` now carries the `-m` message, everything downstream is
unchanged: `message_requiring_validation` validates it, `finalize_commit_message`
+ `ensure_plan_tag` stamp the transient commit, and `run_post_finalize_rewrite`
collapses the plan into one `[<stem>] <subject>` commit. `-m` stays mandatory
(autosquash with no `-m` → `args.squash` stays None → validation rejects).

## Decisions to flag for review

1. **autosquash + `--purge`**: lean SKIP autosquash when `--purge` is explicit
   (purge already rewrites; respect the explicit intent). Reviewer's call.
2. **explicit `--squash "X"`**: always wins over autosquash (only fill when
   `args.squash.is_none()`). Not a real question — just confirming.
3. **per-invocation opt-out**: a `--no-squash` to disable autosquash for one
   finish. Lean: NOT in v1 (set the config per-repo). Add only if needed.
4. **`--global` in this plan**: see above.

## Tests

- config: `finish.autosquash` loads from repo; from global; repo shadows
  global (mirror the `diff` layering tests). `clank config finish.autosquash
  set true` round-trips; `--json` includes it.
- finish-run (reuse `finish_integration.rs` harness): with a repo
  `.clank/config.json` `{"finish":{"autosquash":true}}`, a Ready plan with
  MULTIPLE commits + `finish -m "subject" -m "why"` (on a non-protected branch
  or with `allow_rewrite_protected`) lands ONE commit for the plan, subject
  `[<stem>] subject`; the plan's earlier commits are collapsed.
- autosquash OFF → normal multi-commit finalize (no squash).
- explicit `--squash "X"` still squashes to X even when autosquash on.
- `--purge` with autosquash on is NOT auto-squashed (per decision 1).

## Acceptance criteria

- `finish.autosquash` resolves global→repo (repo shadows), settable via
  `clank config` (repo; global per decision 4), shown in `clank config`.
- With it true, `clank finish <plan> -m "..."` yields ONE tagged commit for
  the plan; with it false, behavior is unchanged.
- `-m` still mandatory + validated + auto-tagged. clippy at baseline; green.

## Deploy

`cargo install --path crates/cli --force`. Optionally set
`~/.clank/config.json` `{"finish":{"autosquash":true}}` to try it globally.
