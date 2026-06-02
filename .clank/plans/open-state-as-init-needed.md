# open-state-as-init-needed

`clank open`'s state currently classifies the repo
structurally (`ClankInitialized` vs `GitWithoutClank`). What
the editor actually wants to know is: **do I need to run
`clank init`?**

Reframe the state around the action the editor (or human) is
about to take, not around what's on disk. Init's job evolves
over time — new managed paths, new `.gitignore` rules, new
permission entries in `.claude/settings.local.json` — and a
repo that was "initialized" months ago might still need init
re-run today.

## Proposed state model

Two values: `ClankInitNeeded` and `ClankReady`. A reason
qualifies as an `InitGap` ONLY when running `clank init`
actually fixes it — every variant must round-trip with
init's own write/repair surface.

Variants on disk today (cross-checked against `cli/init.rs`):

- `MissingClankDir` — `.clank/` doesn't exist.
  (`init.rs::write_scaffold` creates it.)
- `MissingClankGitignore` — `.clank/.gitignore` is absent or
  doesn't match `GITIGNORE_BODY`. (`init.rs::write_scaffold`
  writes/upgrades it.)
- `MissingClaudePermissions` — `.claude/settings.local.json`
  is absent or its `permissions.allow` doesn't include
  `Write(.clank/agents/**)`, `Edit(.clank/agents/**)`, or
  `Read(.clank/agents/**)`. (`init.rs::write_claude_perms`
  tag-merges them.)
- `MissingPostRewriteHook` — `git rev-parse --git-path
  hooks/post-rewrite` resolves to a file that doesn't carry
  `POST_REWRITE_MARKER` (or doesn't exist).
  (`init.rs::write_post_rewrite_hook` installs / refreshes.)

`ClankReady` is the absence of all of the above.

### Not InitGaps

- **Root `.gitignore` carve-outs.** Init currently only
  WARNS via `warn_if_globally_excluded`; it doesn't write
  `/.clank/*` / `!.clank/plans/` / `!.clank/finished/` to
  the root `.gitignore`. Surface this from `clank open` as
  a `warnings` entry on `OpenResponse` (existing field),
  reusing the same probe init uses (`git check-ignore -v`
  against the tracked subpaths). If we later expand init to
  manage the root gitignore, promote this to a real
  InitGap.
- **Agent binding.** A fully-init'd repo with no bound
  agents is `ClankReady` + empty `agents` array. The
  `BindAgent` recommendation is the action, independent of
  init.

## Wire shape (JSON)

`state` stays a string enum to keep the existing flat shape
editors are already parsing:

```json
{
  ...,
  "state": "clank_init_needed",       // or "clank_ready"
  "init_gaps": [
    { "kind": "missing_clank_dir" },
    { "kind": "missing_post_rewrite_hook" }
  ],
  "recommendations": [
    {
      "kind": "clank_init",
      "cwd": "/abs/path",
      "gaps": ["missing_clank_dir", "missing_post_rewrite_hook"]
    }
  ]
}
```

Pinned rules:

- `state` values are exactly the snake_case strings
  `clank_init_needed` and `clank_ready`. No tagged-object
  encoding.
- `init_gaps` is a top-level array of `{ "kind": "<snake>" }`
  objects; omitted (via `skip_serializing_if = "Vec::is_empty"`)
  when state is `clank_ready`.
- The `clank_init` recommendation gains a `gaps: [kind, ...]`
  string array — same kinds in the same order as
  `init_gaps`. Lets editors render "init will: create
  .clank/, install hook" without parsing the top-level
  array.
- Adding a new variant is a forward-compatible additive
  change: editors that don't know the new `kind` still see
  `state == clank_init_needed` and prompt to run init.

## Surfaces touched

- `crates/core/src/...` — `InitGap` isn't shared yet;
  define it in `crates/cli/src/cli/open.rs` for now (single
  consumer). Hoist to core if a second consumer appears.
- `crates/cli/src/cli/open.rs::OpenState` — drop
  `ClankInitialized` and `GitWithoutClank`; add
  `ClankInitNeeded` and `ClankReady`. Compute `InitGap`s by
  probing each artifact `init.rs` manages.
- `crates/cli/src/cli/init.rs` — extract the "what does
  init manage?" facts (paths, expected bodies, marker
  strings) into a small read-only helper module the open
  inspector calls. `init.rs` stays the writer; open is the
  reader.
- `OpenResponse` — add `init_gaps: Vec<InitGap>` (skip
  when empty). Existing `warnings` carries the
  ancestor-gitignore advisory.
- `Recommendation::ClankInit` — add `gaps: Vec<String>`.
  When the state is `ClankReady`, no `ClankInit`
  recommendation appears.
- Human output: state line becomes
  `state: clank_init_needed (N gaps)` with each gap kind
  listed on its own indented line; `clank_ready` otherwise.

## Tests

State + gap probing:

- `init_needed_when_clank_dir_missing` — no `.clank/`,
  expect `init_gaps` contains `missing_clank_dir`.
- `init_needed_when_post_rewrite_hook_absent` — `.clank/`
  + `.clank/.gitignore` + `.claude/settings.local.json`
  present, hook missing → only `missing_post_rewrite_hook`
  in `init_gaps`.
- `init_needed_when_claude_permissions_missing` — no
  `.claude/settings.local.json` → only
  `missing_claude_permissions`.
- `init_needed_when_clank_gitignore_stale` — `.clank/.gitignore`
  exists with old body → `missing_clank_gitignore`.
- `clank_ready_after_full_init` — run `clank init --yes`,
  re-run `clank open`, expect `state == "clank_ready"`,
  `init_gaps` omitted from JSON.
- `clank_ready_unaffected_by_unbound_agents` — fully
  init'd repo, no `.clank/agents/*` dirs; state is
  `clank_ready`, `agents` is empty, only recommendation
  is `bind_agent`.

Wire shape:

- `init_gaps_omitted_when_empty` — assert the JSON has
  no `init_gaps` key when state is ready.
- `clank_init_recommendation_carries_gaps_array` — when
  state is init-needed, the `clank_init` recommendation
  has a `gaps` field matching the top-level `init_gaps`
  kinds in order.

Root-gitignore advisory:

- `warning_for_ancestor_gitignore_excluding_clank_paths`
  — ancestor `.gitignore` excludes `.clank/plans/`; assert
  a `warnings` entry mentions the source file and that no
  `MissingGitignoreEntries`-style `InitGap` is emitted
  (init doesn't fix this).

## Out of scope

- Auto-running `clank init` from the inspector. Inspector
  stays read-only.
- A `clank init --check` mode. Same machinery, different
  invocation; not needed for the editor to function.
- Renaming `clank init` or splitting it into sub-commands.
- Expanding init to manage the root `.gitignore`. Separate
  plan if we want it; until then, root-gitignore problems
  are advisories, not InitGaps.
