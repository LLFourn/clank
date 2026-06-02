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

Replace the binary `ClankInitialized` vs `GitWithoutClank`
with `ClankInitNeeded` carrying the reasons:

- `ClankInitNeeded { reasons: Vec<InitGap> }` — running
  `clank init` would do something useful here. Reasons:
  - `MissingClankDir` — `.clank/` doesn't exist.
  - `MissingGitignoreEntries { entries: Vec<String> }` —
    root `.gitignore` lacks one or more clank rules
    (`.clank/*`, `!.clank/plans/`, `!.clank/finished/`).
  - `MissingClaudePermissions` — `.claude/settings.local.json`
    is absent or doesn't include the `Write/Edit/Read(.clank/agents/**)`
    allow entries.
  - `MissingPostRewriteHook` — `.git/hooks/post-rewrite`
    isn't installed or doesn't carry the clank marker.
  - `MissingClankGitignore` — `.clank/.gitignore` is absent
    or stale relative to the current canonical body.
- `ClankReady` — none of the above; init has nothing to do.
  (Distinct from "agent X is bound to a session" — that's
  separate, surfaced by the existing `agents` array.)

Agent registration is NOT an init gap. If the editor wants
to launch agent `claude` and `clank.agents` doesn't include
it, that's a `BindAgent` recommendation regardless of init
state. Don't conflate "repo wiring needs update" with
"this session needs to bind."

## Why this shape

- Action-oriented for editors: read `state == ClankInitNeeded`
  → "run `clank init`." No structural inference required.
- Forward-compatible: when init starts managing a new
  file/path, add a new `InitGap` variant. Existing editors
  ignore unknown variants and still surface `ClankInitNeeded`.
- Idempotent fit: `clank init` is already designed to be
  re-run safely; this state model is its natural read side.
- Captures the `mkdir .clank` corner cleanly: `.clank/`
  exists but `MissingGitignoreEntries` / `MissingClankGitignore`
  / `MissingClaudePermissions` light up → still
  `ClankInitNeeded`, not falsely `Ready`.

## Surfaces touched

- `crates/cli/src/cli/open.rs::OpenState` — replace
  `ClankInitialized` / `GitWithoutClank` with the two new
  variants. Compute `InitGap`s by probing each managed
  artifact.
- `crates/cli/src/cli/init.rs` — extract the
  "what does init manage?" facts into a small read-only
  helper the open inspector can call. Source of truth stays
  in init.rs; open re-uses it instead of duplicating path
  lists.
- Recommendations: when `ClankInitNeeded`, emit a single
  `ClankInit { cwd, reasons }` recommendation. `BindAgent`
  is independent.
- Human output: state line becomes
  `state: clank_init_needed (3 gaps)` with the gap kinds
  listed underneath; `clank_ready` otherwise.

## Tests

- `init_needed_when_clank_dir_missing` — no `.clank/`,
  expect `ClankInitNeeded { MissingClankDir }`.
- `init_needed_when_gitignore_missing_entries` — `.clank/`
  exists but root `.gitignore` lacks `.clank/*` carve-outs.
- `init_needed_when_claude_permissions_missing` — no
  `.claude/settings.local.json`.
- `init_needed_when_post_rewrite_hook_absent` — `.clank/`
  present, hook missing.
- `clank_ready_after_full_init` — run `clank init --yes`,
  re-run `clank open`, expect `ClankReady` with no gaps.
- `clank_ready_unaffected_by_unbound_agents` — repo is fully
  init'd but no agents bound; state is still `ClankReady`,
  agents array is empty, recommendation is `BindAgent` not
  `ClankInit`.

## Out of scope

- Auto-running `clank init` from the inspector. Inspector
  stays read-only.
- A `clank init --check` mode. Same machinery, different
  invocation; not needed for the editor to function.
- Renaming `clank init` or splitting it into sub-commands.
