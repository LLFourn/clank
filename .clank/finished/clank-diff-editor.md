# clank-diff-editor
# `clank diff <plan|range>` — universal editor launcher for plan/commit diffs, with optional agent-supplied prompt + focus hints. Configured per-user (and per-repo) in `.clank/config.json`.

## Problem

lloyd 2026-06-05:

> I want clank agents to have a universal way to open a diff in an editor for the user. This would be in ~/.config.json. The agent or the user would go "clank diff <plan-name>" or even a commit range. This call could have a --wait option which waits for the process spawned to be closed before continuing. Otherwise it just opens and continues.
>
> To the tool internally I suppose the right api is to pass the plan name (if it's not an adhoc commit range), the commit range, maybe even a prompt from the agent about what the user should be reviewing. Maybe even particular sections of certain files that should be of interest. The idea is that it could open emacs in a certain way with all the panes arranged according to what the command was told was important.

Today an agent that wants a human to glance at a diff has two bad options:

1. Print the diff inline into its tool transcript (loses fidelity, no editor jumps, no syntax navigation).
2. Ask the user to manually `git diff <range>` in their own terminal (high friction; the agent's context — "look at the lifecycle hook in `state.rs:120`" — is lost).

Clank already has the structured surface to make this push-button:

- Plans → commit ranges via `crate::cli::plan_resolve::resolve_plan` (already used by `clank log`, `clank purge`, `clank finish`).
- A typed `.clank/config.json` schema (`crate::cli::config::Config`, extended throughout `agent-add-cli-and-repo-scope`).
- A working "compose-launch + exec" pattern in `crate::cli::agent::compose_launch` (declaration `launch: Option<LaunchConfig>` → executable + args + env), which is a near-perfect template for the editor-launch path.

What's missing: a subcommand that wires plan-resolve + a configured editor invocation together, with an env channel for agent-supplied context (`--prompt`, `--focus`).

## Verified before promotion (2026-06-05)

Concrete pointers checked in tree at the time of writing — re-confirm at promote-time, code moves fast:

- **Plan-resolve surface**: `crates/cli/src/cli/plan_resolve.rs:15-37` (`resolve_plan`) takes `(state, expected_basename, plan_arg: Option<&str>)` and returns a `PlanKey`. `crates/cli/src/cli/plan_resolve.rs:92-109` (`parse_arg`) handles bare-stem / `.md` / `<basename>/<stem>.md` / `.clank/plans/<stem>.md` forms. `clank log` and `clank purge` already consume it (`crates/cli/src/cli/log.rs:21-30`, `crates/cli/src/cli/purge.rs:62`).
- **Plan-or-range pattern**: `clank log` already accepts BOTH a `--plan` flag AND a positional `<range>` arg (`crates/cli/src/cli/log.rs:21-42`). They are independent: `--plan` filters, `<range>` bounds. We do NOT want that shape — `clank diff` takes ONE positional that is EITHER a plan OR a range. See "Argument parsing" under Open questions.
- **Range parsing**: `crates/cli/src/cli/log.rs:102-117` (`parse_range`) already handles `<from>..<to>` and bare `<sha>` (latter expands to `<sha>^..HEAD`). Reuse or factor out — the diff command wants `<from>..<to>` semantics most of the time.
- **Plan → commit list (NOT range)**: codex review of cba9249 caught that a chronological range would include interleaved commits from other concurrently-active plans. The diff command needs the plan-attributed commit LIST, not a range. `state.fold.plans[plan_key].commits` carries this for active plans. For finished plans: `preview::build_rewrite_preview` returns `Vec<RewriteCommit>` over `[intro, head]`; **filter to `!commit.foreign`** to get the plan-attributed set (codex review of 0859200 — `RewriteCommit { foreign: bool }` at `crates/core/src/api.rs:144-158` marks interleaved non-plan commits; passing them through unfiltered reintroduces the same leakage on finished plans). Helper: `plan_resolve::commits_for_plan(state, &plan_key) -> Vec<CommitSha>`. See Phase 3.
- **Config schema is typed today** at `crates/cli/src/cli/config.rs:12-43` (`Config { review, hooks }`) but the on-disk layer is read via `apply_layer` (`config.rs:245-292`) which uses an intermediate `ConfigFile` struct. The `agents` field is read via separate typed wrappers (`RepoAgentsFile` / `UserAgentsFile`, `config.rs:128-141`). A new `diff` section follows the same pattern — typed Rust struct, additive to `Config`, additive to the on-disk schema.
- **LaunchConfig exists and is the right shape for "command + args + env"**: `crates/core/src/agent_config.rs:67-83`. It's already used by `DefaultAgent.launch` in the declaration. Reusing it for the editor config keeps one launch-profile schema across the codebase.
- **Compose+exec pattern lives at** `crates/cli/src/cli/agent.rs:199-242` (`compose_launch`, `ComposedLaunch`) and `agent.rs:277-301` (`exec_composed` — `unix::process::CommandExt::exec` on unix, `spawn().wait()` on non-unix). The unix `exec` path REPLACES the current process; the diff command wants `spawn()` semantics (clank stays alive to print messages / return control), so this is "inspired by" not "share with".
- **POC for the same shape exists** at the repo root: `open-worktree.sh` reads structured config from `clank open --json` and constructs a zellij KDL layout from it. Not a direct precedent for diff but illustrates the "structured config → editor pane arrangement" pattern. `clank-open-zellij` (queued at `.clank/queue/600-clank-open-zellij.md`) is the related "open multi-pane editor" plan.

## Approach (phased)

### Phase 1: Editor config schema in `.clank/config.json`

Add a `diff` section to the typed config. Sketched shape (pin exact field names at promote-time):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct DiffConfig {
    /// Editor launch profile. `None` = `clank diff` errors with
    /// "no editor configured; set diff.editor in
    /// ~/.clank/config.json". Default-empty avoids accidentally
    /// launching `vi` or `$EDITOR` (which would surprise users
    /// who set `$EDITOR` for git commit-msg editing but expected
    /// a different review surface for diffs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<LaunchConfig>,
    /// Default `--wait` behavior. `Some(true)` = wait by default,
    /// `--no-wait` overrides. `Some(false)` = fire-and-forget,
    /// `--wait` overrides. `None` = fire-and-forget (current
    /// default per Lloyd's wording: "otherwise it just opens and
    /// continues").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<bool>,
}
```

Layered loader same as the existing review/hooks settings: user-scope `~/.clank/config.json` provides a baseline; repo-scope `<repo>/.clank/config.json` can override (this lets the repo say "use magit for THIS project even though my user-scope says vscode"). Same lossy semantics as the rest of `apply_layer` — malformed editor config logs a warning and falls back to "no editor configured."

**Typed from day one** per `typed-config-dogfood`. No raw JSON literals in tests; construct `DiffConfig` values and serialize via `serde_json::to_string_pretty`.

### Phase 2: `clank diff` subcommand surface

Add `clank diff` to `crates/cli/src/cli/mod.rs`. Shape:

```text
clank diff [<plan|range>] [--plan <plan>] [--range <range>]
           [--prompt <text>] [--focus <file>[:<lines>]]...
           [--wait | --no-wait]
           [--print] [--repo <path>]
```

- **Positional `<plan|range>`**: auto-resolves. Disambiguation policy under Open questions.
- **`--plan` / `--range`**: explicit forms; mutually exclusive with each other and with the positional. Useful when the heuristic guesses wrong.
- **`--prompt <text>`**: agent-supplied free-form hint about what's interesting. Passed to the editor via env var (see Phase 4).
- **`--focus <file>[:<lines>]`** (repeatable): pin specific file regions. Forwarded to the editor as a structured list (see Phase 4). Line syntax pinned in open questions.
- **`--wait` / `--no-wait`**: override the configured default. Mutually exclusive.
- **`--print`**: dry-run; print the composed editor command (program + argv + env additions) and exit 0. Mirrors `clank agent start --print` (see `agent.rs:191-194`). Critical for tests and for the "verify my editor config" loop.
- **`--repo`**: standard override (same as every other subcommand).

If no positional is given and neither `--plan` nor `--range` is set: infer the single active plan via `resolve_plan(state, basename, None)` — same fallback `clank log` uses without `--plan` (`log.rs:21-30`).

### Phase 3: Plan → commit list (NOT range). Range stays as range.

**The interleaving problem** (codex review of cba9249): clank explicitly supports multiple concurrently-active plans (`all-reviewers-gate` finished; multiple `plans/*.md` legal). Commits from plans A and B interleave in chronological order. A plain `intro_sha^..HEAD` git range for plan A would include plan B's commits. `clank diff <plan>` returning a chronological range is "show me everything between A's intro and now," not "show me what A did." Two different semantics; only the second matches the agent's mental model ("Lloyd, look at the changes I made for THIS plan").

**Resolution**: `clank diff <plan>` produces the COMMIT LIST attributed to the plan, NOT a range. `clank diff <range>` keeps range semantics for ad-hoc use. The editor decides how to render each.

The state-fold already tracks per-plan commit timelines (this is what `clank log <plan>` filters by). Two source paths depending on plan status:

- **Active plan**: `state.fold.plans[plan_key].commits` — already plan-attributed by construction.
- **Finished plan**: re-derive via `crate::preview::build_rewrite_preview`. **CRITICAL**: filter the returned `Vec<RewriteCommit>` to `!c.foreign` before extracting SHAs. The preview returns ALL commits in `[intro_sha, head_sha]` including interleaved non-plan commits (marked `foreign: true`). The active-plan path doesn't have this concern because the fold's per-plan timeline tracks attribution natively, but the finished-plan path goes through the preview which is range-based. Codex review of 0859200 caught this; it's the same interleaving bug, just in the finished-plan branch.

Add a helper next to `resolve_plan`:

```rust
// in plan_resolve.rs or a new commits.rs sibling
pub fn commits_for_plan(state: &RepoState, key: &PlanKey)
    -> anyhow::Result<Vec<CommitSha>>;
```

Returns the chronologically-ordered plan-attributed commits.

**For a raw range** (`HEAD~3..HEAD` or `abc..def`): keep range semantics. Reuse `log::parse_range` (factor out to `plan_resolve` or a new `range_parse` module so log and diff share it — same `(Option<CommitSha>, CommitSha)` output).

**Two semantically-distinct primitives surface to the editor** (via Phase 4 env vars below):

| User invocation | Semantic | Surfaces |
|---|---|---|
| `clank diff <plan>` | plan-exact | `CLANK_DIFF_KIND=plan`, `CLANK_DIFF_COMMITS=sha1,sha2,...`, `CLANK_DIFF_PLAN=<stem>`. NO `CLANK_DIFF_RANGE` (would mislead). |
| `clank diff <range>` | ad-hoc range | `CLANK_DIFF_KIND=range`, `CLANK_DIFF_RANGE=<from>..<to>`. NO `CLANK_DIFF_COMMITS`. |

Editors that want a SINGLE-PATCH view (rather than per-commit) can opt-in via a `{patch_file}` template variable in `LaunchConfig.args` (see Phase 4); when present, clank synthesizes a tempfile patch from `CLANK_DIFF_COMMITS` (cherry-pick-style stacked diff) or from the range (`git diff <from>..<to>`), and substitutes the path.

### Phase 4: Compose the editor invocation

Mirrors `agent::compose_launch` (`agent.rs:207-227`) at the structural level. The composed value:

```rust
struct ComposedDiffLaunch {
    program: String,
    args: Vec<String>,
    env_overrides: BTreeMap<String, String>,
    wait: bool,
}
```

Template substitution in `args` — different variables available depending on `CLANK_DIFF_KIND`:

Common (always available):
- `{repo}` → repo root absolute path.

For `CLANK_DIFF_KIND=range` (`clank diff <range>`):
- `{range}` → `<from>..<to>` literal.
- `{from}`, `{to}` → individual SHAs.

For `CLANK_DIFF_KIND=plan` (`clank diff <plan>`):
- `{plan}` → plan stem.
- `{commits}` → comma-separated SHA list (same as `CLANK_DIFF_COMMITS`).
- `{first_commit}`, `{last_commit}` → endpoints of the commit list.

For both (opt-in by use; only synthesized if the template appears):
- `{patch_file}` → absolute path to a tempfile containing a synthesized patch. For range: `git diff <from>..<to>`. For plan: stacked `git format-patch`-style concatenation of the plan's commits. The tempfile is cleaned up after the editor exits (under `--wait`) or after a `clank-managed` cleanup timeout (under fire-and-forget; specific timeout pinned at implementation).

Using a template variable that doesn't apply to the current `CLANK_DIFF_KIND` → error at compose time ("`{range}` not available when launching via `--plan`"). Avoids silent confusion.

Env additions (in addition to whatever `LaunchConfig.env` supplies):

- `CLANK_DIFF_KIND` = `"plan"` or `"range"`. ALWAYS set.
- `CLANK_DIFF_REPO` = repo root. ALWAYS set.
- `CLANK_DIFF_PROMPT` = the `--prompt` text if set.
- `CLANK_DIFF_FOCUS` = newline-separated `<file>:<lines>` entries.
- Kind-specific:
  - When `CLANK_DIFF_KIND=plan`: `CLANK_DIFF_PLAN=<stem>`, `CLANK_DIFF_COMMITS=<comma-separated SHAs in chronological order>`.
  - When `CLANK_DIFF_KIND=range`: `CLANK_DIFF_RANGE=<from>..<to>`.
- The kind-specific env vars from the OTHER kind are explicitly NOT set (no `CLANK_DIFF_RANGE` for plan invocations, no `CLANK_DIFF_COMMITS` for range invocations). This is load-bearing: a misconfigured editor command that reads `CLANK_DIFF_RANGE` would silently produce wrong output for a plan invocation if we set both.

The env-var channel is the most universal — every editor's launch shell can read env. Stdin piping is reserved for editors that explicitly want raw patch text (out of scope for v1).

### Phase 5: Spawn semantics (`--wait` vs fire-and-forget)

Two code paths, both via `std::process::Command`:

- **Fire-and-forget** (default): `spawn()` + drop the `Child` handle. The editor outlives clank. On unix this works as long as we don't hold stdio handles open in a way that pins the child. Detach stdio (`Stdio::null()` for stdin/stdout/stderr) unless the user wants to see editor output inline; that's a corner the plan can defer.
- **`--wait`**: `spawn().wait()`. Clank's exit code = editor's exit code. Sigint cleanup: if clank receives SIGINT during the wait, forward to the child and wait for cleanup (or document that it's the editor's responsibility to handle SIGINT — emacsclient does, vim does, vscode `--wait` does).

This is intentionally NOT exec-replacement (unlike `agent start`). Clank's diff command always returns to its caller so the agent can react to a non-wait launch or to the wait-completed exit.

### Phase 6: CLI plumbing

- Wire `DiffArgs` into `mod.rs` alongside `LogArgs` / `PurgeArgs`.
- Add a `crates/cli/src/cli/diff.rs` module with `pub async fn run(args: DiffArgs) -> anyhow::Result<()>`.
- Register in the top-level `clap` dispatch (parent enum lives in `crates/cli/src/main.rs` — verify path at promote-time, not re-read here).
- Doctor: optional new diagnostic — "diff.editor configured but `command` not on PATH" → Warn. Low priority; can be a follow-up plan if it complicates this one.

### Phase 7: Tests

Listed in Tests section below.

## Resolved at promotion (2026-06-05)

Pinning the eight design decisions the plan body has carried as "tentative" through three review rounds. Codex's cba9249 + 0859200 + 170be64 catches resolved questions 5 and (via the foreign-filter fix) the interleaved-plan family of edge cases under 8. The remaining items were pure design picks waiting for a decision; ruthless's 170be64 review correctly pushed for closing them before implementation.

1. **Positional disambiguation: `clank diff <arg>` parsing**. **Pinned: option (a)**. Try `plan_resolve::parse_arg` + `resolve_plan` first; on `PlanNotFound`, try `parse_range`; on both failures, error with: ``<arg>` is not a known plan and is not a valid git range. Available plans: <list>. To pass a literal range, use `--range <arg>`.`` Rationale: the common case (`clank diff foo`) needs zero friction; the explicit `--plan`/`--range` flags exist as escape hatches when the heuristic fails.

2. **Editor config shape: reuse `LaunchConfig`**. **Pinned**. Wider docstring change happens IN this plan's Phase 1: rewrite `LaunchConfig`'s docstring (currently mentions claude/codex args ordering) to describe it as a general-purpose launch profile, with the agent-start args ordering called out as one specific consumer. If editor needs grow beyond what `LaunchConfig` covers (focus-arg builder, multi-pane KDL output, etc.), split into a sibling struct in a follow-up.

3. **`--focus` syntax: `file:start-end`**. **Pinned: option (b)**. CLI flag: `--focus path/to/file.rs:10-42`. Emitted verbatim into `CLANK_DIFF_FOCUS` (newline-separated for multiple `--focus` flags). Editor's responsibility to convert to whatever its native form is (e.g. emacs `L10-L42`). Single-line form `--focus path/to/file.rs:42` is shorthand for `42-42`; whole-file form `--focus path/to/file.rs` (no `:`) is shorthand for "the whole file."

4. **`--prompt` delivery: env always + `{prompt}` opt-in template**. **Pinned**. `CLANK_DIFF_PROMPT` is set unconditionally when `--prompt` is passed. Additionally, `{prompt}` is a recognized template variable in `LaunchConfig.args` (both kinds — plan and range); users who want the prompt inlined into the editor command can use it. If `{prompt}` appears but `--prompt` isn't passed, substitute with empty string (NOT an error — agents may have configs that always include the template but not always pass `--prompt`).

5. **What does the editor receive** — **RESOLVED** (codex cba9249 forced the call). Editors get two semantically-distinct primitives via env vars: `CLANK_DIFF_COMMITS` for plan invocations (plan-attributed SHA list), `CLANK_DIFF_RANGE` for range invocations (raw `<from>..<to>` string). Plus an opt-in `{patch_file}` template variable that synthesizes a tempfile patch for editors that want a single-patch view. Stdin piping deferred. See Phase 3 + Phase 4 for the table of which env vars are set under which invocation.

6. **SIGINT handling under `--wait`: forward to child**. **Pinned**. Clank installs a SIGINT handler during the wait that forwards SIGINT to the child PID and continues waiting. The child (emacsclient, vscode `--wait`, vim) handles its own cleanup. After the child exits, clank exits with the child's exit code (or 130 if killed by signal). Document this in `clank diff --help`: "ctrl-c during `--wait` forwards SIGINT to the editor; the editor decides cleanup."

7. **Repo-scope override: field-by-field layering**. **Pinned**. Repo-scope `diff` section overrides user-scope field-by-field via the existing `apply_layer` mechanism (same shape as `review` / `hooks`). Concretely: if user-scope sets `diff.editor.command = "vim"` and `diff.wait = false`, and repo-scope sets `diff.editor.command = "emacsclient"`, the effective config is `diff.editor.command = "emacsclient"` (overridden) + `diff.editor.args/env` from user-scope (not overridden because repo-scope didn't set them) + `diff.wait = false` (user-scope). NOT the `agents`-style REPLACE semantic; that semantic is specific to list-shaped data where empty-list-means-disable.

8. **Plan range edge cases**. **Pinned** (codex 0859200's foreign-filter fix already addressed the "interleaved plans" subset; here are the rest):
   - **Plan with no commits attributed yet** (no intro committed) — `resolve_plan` errors with "plan not found" already, no special handling needed.
   - **Plan with intro committed but no revisions / implementation yet** — `commits_for_plan` returns `vec![intro_sha]`. `CLANK_DIFF_COMMITS=<intro_sha>`; `{patch_file}` synthesizes the diff of that single commit. Editor renders normally.
   - **Plan deleted (`PlanDeleted` log event in fold history)** — `resolve_plan` returns `PlanNotFound` because deleted plans are absent from `state.fold.plans` AND `state.fold.finished_plans`. Error message matches the active/finished branch. (Resurrecting a deleted plan to diff it requires `clank unfinish` or the demote/restore path; out of scope here.)
   - **Plan finished AND purged (`.clank/` artifacts removed from history via `clank purge`)** — the finished-plan record at `state.fold.finished_plans` may still exist depending on whether finalize was purged too. If still recorded: `commits_for_plan` finds the intro + finalize SHAs but the underlying commits may have been rewritten. The `build_rewrite_preview` path errors cleanly in this case (commits-by-SHA lookup fails); pass that error through with context: "plan `<stem>` is finished but its commits no longer exist in history (possibly purged). To diff a purged plan, restore from the orphan ref or skip."
   - **Plan with finalize but no other commits** — same as intro-only; the helper returns a single SHA and downstream proceeds normally.

## Out of scope

- Multi-editor support per repo (one config, one editor — if you want both vim and emacs, swap config per session).
- Editor-side integration helpers (no shipped emacs lisp / vscode extension that knows how to consume `CLANK_DIFF_*`). Users wire their own.
- Diff rendering inside clank itself. `clank diff` is a LAUNCHER, not a renderer. `clank-html` already covers "render diff as a web page."
- `--commit <sha>` shorthand for "ad-hoc single commit." Follow-up. (Note: `clank diff <range>` already covers `<sha>^..<sha>` if the user spells it out.) Per the Phase 3 pivot, plan invocations are commit-list based; range invocations are range based. Single-commit ergonomics is a small additional surface, not a missing primitive.
- Pinning a specific worktree (multi-worktree diff). v1 uses the cwd's repo + working tree.
- Doctor diagnostic for "editor binary not on PATH." Deferred; useful but not load-bearing.
- A repo-scope `diff.editor` REPLACE-vs-merge semantics decision that requires changing `apply_layer`. If the existing layered semantics work, use them; if not, defer.

## Acceptance

**Config schema (Phase 1)**

- `Config` carries a `diff: DiffConfig` field; `DiffConfig` carries `editor: Option<LaunchConfig>` + `wait: Option<bool>`.
- `apply_layer` reads `diff` from both user-scope and repo-scope `.clank/config.json`.
- Schema is typed from day one; no hand-rolled JSON in any new test.
- `clank config diff.editor` / `clank config diff.wait` enumerated in `KEY_CATALOG` (matches the existing pattern at `config.rs:304-361`). `clank config` enumerates them.

**Subcommand surface (Phase 2 + 6)**

- `clank diff <plan>` resolves the plan via `plan_resolve::resolve_plan` and produces the plan-attributed commit list via `commits_for_plan` (NOT a range; codex review of cba9249 — the plan-attributed commit list is the only correct primitive when plans interleave).
- `clank diff <range>` (raw `<from>..<to>` or bare SHA) parses via the shared range parser.
- `clank diff <plan>` in a repo with interleaved commits from another plan does NOT include the other plan's commits in `CLANK_DIFF_COMMITS` or in the synthesized `{patch_file}`.
- `clank diff` (no args) infers the single active plan, errors with candidates listed if ambiguous (same UX as `clank log` without `--plan`).
- `clank diff --plan <x>` and `clank diff --range <x>` work as explicit overrides; mutually exclusive with each other and with the positional.
- `clank diff --print` emits the composed launch line (program + argv + env) on stdout, exits 0, spawns nothing.

**Editor launch (Phase 4 + 5)**

- Composed launch substitutes the kind-specific template variables in `LaunchConfig.args`. For range: `{range}`, `{from}`, `{to}`, `{repo}`. For plan: `{plan}`, `{commits}`, `{first_commit}`, `{last_commit}`, `{repo}`. For both: `{patch_file}` synthesizes a tempfile on use.
- Using a template variable that doesn't apply to the current kind errors at compose time with a clear message.
- Env additions: `CLANK_DIFF_KIND` and `CLANK_DIFF_REPO` ALWAYS set. Range kind adds `CLANK_DIFF_RANGE`. Plan kind adds `CLANK_DIFF_PLAN` + `CLANK_DIFF_COMMITS`. The other kind's env vars are explicitly NOT set (no leakage). `CLANK_DIFF_PROMPT` set when `--prompt`; `CLANK_DIFF_FOCUS` when `--focus`.
- Default spawn = fire-and-forget; `--wait` blocks until exit; `--no-wait` overrides config-default `wait: true`.
- Errors clearly when `diff.editor` is unconfigured: "no editor configured; set diff.editor.command in ~/.clank/config.json".

**Workspace**

- `cargo test --workspace` passes.
- No raw JSON literals introduced in any new test (per `typed-config-dogfood` direction).

## Tests

### Config schema

1. `diff_config_round_trips_with_editor_and_wait`: build `DiffConfig { editor: Some(LaunchConfig { ... }), wait: Some(true) }`, serialize, deserialize, assert equality.
2. `diff_config_defaults_when_section_absent`: `.clank/config.json` with no `diff` key → `cfg.diff` is default (`editor: None`, `wait: None`).
3. `diff_config_repo_scope_overrides_user_scope`: user-scope `editor: vim`; repo-scope `editor: emacsclient`; loaded `cfg.diff.editor.command == "emacsclient"`.
4. `diff_config_malformed_ignored`: malformed JSON in the `diff` section logs a warning and `cfg.diff` falls back to default — same lossy semantics as the rest of `apply_layer`.

### Plan → commit list resolution

5. `commits_for_plan_active_returns_plan_attributed_chronological`: repo with an active plan + 3 plan commits; `commits_for_plan` returns those 3 SHAs in chronological order.
6. `commits_for_plan_finished_returns_intro_to_finalize_set`: repo with a finished plan; helper returns the full set including the finalize commit.
7. `commits_for_plan_excludes_interleaved_other_active_plan_commits` **(codex cba9249)**: repo with plan A (intro + 1 revise), plan B (intro + 1 revise) interleaved chronologically; `commits_for_plan(state, key_A)` returns ONLY plan A's 2 SHAs. Exercises the active-plan path (fold's per-plan timeline).
8. `commits_for_plan_finished_excludes_interleaved_foreign_commits` **(codex 0859200)**: repo where plan A was finished after plan B interleaved commits between A's intro and A's finalize. `commits_for_plan(state, key_A)` returns ONLY plan A's commits (the finished-plan path goes through `build_rewrite_preview` which returns `RewriteCommit` entries; the helper must filter `!c.foreign` before extracting SHAs). This is a different code path than the active-plan case AND a different regression target — codex specifically called out that the active-plan-only test would not catch this.
9. `commits_for_plan_unknown_errors`: plan key not in fold → error matches "plan not found".

### CLI surface

10. `clank_diff_positional_resolves_plan`: `clank diff foo --print` resolves `foo` as a plan; composed env includes `CLANK_DIFF_KIND=plan` + `CLANK_DIFF_COMMITS=<sha1>,<sha2>,...` + `CLANK_DIFF_PLAN=foo`. Does NOT include `CLANK_DIFF_RANGE`.
11. `clank_diff_positional_resolves_range`: `clank diff HEAD~2..HEAD --print` skips plan resolution; composed env includes `CLANK_DIFF_KIND=range` + `CLANK_DIFF_RANGE=HEAD~2..HEAD`. Does NOT include `CLANK_DIFF_COMMITS`.
12. `clank_diff_positional_unknown_errors_clearly`: `clank diff not-a-plan-not-a-range --print` errors with a message that mentions both interpretations ("`not-a-plan-not-a-range` is not a known plan and is not a valid git range").
13. `clank_diff_no_args_infers_single_active_plan`: one active plan in repo; `clank diff --print` resolves it.
14. `clank_diff_no_args_ambiguous_lists_candidates`: two active plans; `clank diff` errors with both listed (same shape as `clank log`'s ambiguity error).
15. `clank_diff_plan_kind_template_var_used_in_range_invocation_errors` and inverse: `LaunchConfig.args = ["--commits", "{commits}"]` invoked with `clank diff <range>` → compose-time error naming the wrong-kind template variable.

### Launch composition

16. `clank_diff_substitutes_range_template_in_args`: range invocation; `LaunchConfig { args: ["--eval", "(magit-diff '{range}')"] }` → composed args contain `(magit-diff 'HEAD~2..HEAD')`.
17. `clank_diff_substitutes_plan_commits_template_in_args`: plan invocation; `LaunchConfig { args: ["--eval", "(magit-show-commits '{commits}')"] }` → composed args contain `(magit-show-commits 'sha1,sha2')`.
18. `clank_diff_patch_file_template_synthesizes_tempfile_path`: `LaunchConfig { args: ["{patch_file}"] }` → clank writes synthesized patch to a tempfile, composed args contain the absolute path. Tempfile path can be opened.
19. `clank_diff_print_outputs_composed_line`: `--print` emits program + shell-quoted args on stdout and env additions on stderr (mirrors `agent start --print`).
20. `clank_diff_unconfigured_editor_errors`: no `diff.editor` set → error names the config key + path.
21. `clank_diff_wait_flag_overrides_config_default_false`: config has `wait: false`; `--wait` flips composed `wait` to true.
22. `clank_diff_no_wait_flag_overrides_config_default_true`: config has `wait: true`; `--no-wait` flips composed `wait` to false.
23. `clank_diff_focus_syntax_round_trips`: `--focus path/to/file.rs:10-42 --focus other.rs:5` produces `CLANK_DIFF_FOCUS=path/to/file.rs:10-42\nother.rs:5-5` (single-line `:5` shorthand expanded to `:5-5`). `--focus path.rs` (no `:`) produces `CLANK_DIFF_FOCUS=path.rs` (whole-file form). Verifies the pinned option (b) syntax under question 3.

### Smoke (gate-able by feature)

24. `clank_diff_spawns_configured_editor_smoke`: with `diff.editor.command = "true"` (POSIX no-op binary), `clank diff <range>` (fire-and-forget) returns Ok and the test process doesn't block. With `--wait`, the same returns Ok after `true` exits.

## Related history

- `clank-log-v2` (FINISHED): introduced `clank log <range>` + plan-or-range patterns. Range parser (`log::parse_range`) is the reuse target for Phase 3.
- `clank-open-inspector` (FINISHED) + `clank-open-inspector-impl` (FINISHED): `clank open` family of commands. Different surface (path classifier, not diff launcher), but the "config-driven editor target" idea is adjacent.
- `agent-config-and-start` (FINISHED `18a5e78`): shipped `LaunchConfig` + `compose_launch` + `--print` UX. `clank diff` reuses the launch-profile shape and mirrors `--print` semantics.
- `agent-add-cli-and-repo-scope` (active): typed config evolution + repo-scope overrides for `agents`. Same layering pattern applies to `diff`. Promote AFTER this lands so the config-write helpers are stable.
- `typed-config-dogfood` (queued, `.clank/queue/500-typed-config-dogfood.md`): all new config types ship typed + serde-driven. This plan honors that direction from day one.
- `clank-open-zellij` (queued, `.clank/queue/600-clank-open-zellij.md`): sibling "config-driven editor surface." Different scope (multi-agent layout vs single-diff launch) but worth landing both before the user assembles the full review-loop UX.
- `open-worktree.sh` (untracked POC at repo root): illustrates the "structured config → editor pane layout" pattern. Not a precedent for diff specifically, but the same mental model.

## Suggested ordering

Land AFTER `agent-add-cli-and-repo-scope` (active) so `LaunchConfig` reuse is stable and the typed-config patterns are settled. Independent of `clank-open-zellij` (queued) — both can land in either order. Probably land BEFORE or alongside `typed-config-dogfood` since this plan adds a new typed section as a fresh datapoint for the dogfood migration.
