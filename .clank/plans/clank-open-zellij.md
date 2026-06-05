# clank-open-zellij
# `clank open zellij` — auto-generate a zellij layout (KDL) that spawns master + reviewers in tabbed panes via `clank agent start`. Replaces the POC at open-worktree.sh.

## Problem

Today, getting a multi-agent layout running in zellij is a manual script (`open-worktree.sh` in the repo root, untracked). It hardcodes the master agent and parses `clank open --json` with `jq` to enumerate reviewers, then constructs a zellij layout-string. Works but:

- Lives outside the binary; new users have to discover/copy it.
- Per-agent launch behavior is hardcoded (just runs the bare tool). Doesn't consume `AgentConfig.launch` profiles.
- Master selection is a constant in the script, not a config lookup.
- Reviewer set is parsed from clank's JSON output but the actual launch line is composed in shell with `jq`.

With `clank agent start <name>` shipped (FINISHED `agent-config-and-start`), the per-pane launch line is one command. The layout is the only thing missing.

## Verified before promotion (2026-06-05)

- **POC script at `open-worktree.sh`** read end-to-end. Uses bare tool (`claude`, `codex`) + seed prompt asking each agent to run `clank as <label>` to self-bind. Wraps panes in `zellij:tab-bar` and `zellij:status-bar` plugins. Tab spawned via `zellij action new-tab --cwd <wt> --name <name> --layout-string <KDL>`.
- **POC creates a worktree** as part of its flow (`.clank/worktrees/<name>`) + runs `clank init` in it. This plan scopes `clank open zellij` to JUST the layout — worktree creation is a separate concern (and the POC stays functional as a wrapper for users wanting the bundled flow).
- **Codex allow-list** at `crates/cli/src/cli/setup.rs:151` uses bare `prefix_rule(pattern=["clank"], decision="allow")` — whitelists every `clank <subcommand>` without enumerating individual subcommands. **Rename does NOT require allow-list updates.**
- **No `clank open` references in `crates/cli/src/cli/setup_assets/*.md`** (claude_skill.md, codex_skill.md). Skill files don't need rename migration.
- **Current `clank open <path>`** at `crates/cli/src/cli/open.rs` with `OpenArgs { path: String, json: bool }` (`mod.rs:165-172`). Dispatched at `main.rs:109`. ~5 test usages in `crates/cli/tests/open_integration.rs`.
- **Agent enumeration source**: `clank::cli::config::load_merged_agents` (post `agent-add-cli-and-repo-scope`, FINISHED `1cf029b`). The declaration is the source of truth for the agent set; the legacy `load_all_agent_configs` skeleton scan is fallback only.
- **`clank agent start <name>`** at `crates/cli/src/cli/agent.rs` (post `agent-config-and-start`, FINISHED `18a5e78`). Composes the launch line from the merged declaration's `tool` + `launch` profile and the agent's bound session.

## Approach

`clank open` becomes a clap `Subcommand` container with two siblings:

- `clank open dry <path> [-j]` — **renamed from today's `clank open <path>`** per lloyd 2026-06-05. The existing path classifier; emits JSON the POC + tools consume. Breaking change to the existing CLI: every caller must update. Migration is mechanical — insert `dry` between `open` and the path.
- `clank open zellij` — new layout spawner described below.

### `clank open zellij` behavior

1. **Determine the agent set.** Call `load_merged_agents(repo, home.as_deref())?` (the declaration is authoritative). Role classification:
   - **Zero-master case**: error "no master agent registered for this repo — register one with `clank agent add <label> --role master`."
   - **Multi-master case (pinned per ruthless 7a58d12)**: error with diagnostic naming the duplicates. "multiple master agents registered: <list>. zellij layout requires exactly one master. Resolve with `clank agent set-role <label> reviewer` to demote a duplicate." Fail-loud reasoning matches `agent-config-and-start`'s unbound-session refusal — a multi-master declaration is a config error the user should fix once, not paper over on each invocation.
   - **Exactly-one-master case**: that label is the master; everything else with `role = Reviewer` becomes a reviewer pane in declaration order.
2. **Construct the layout (KDL string).** Composition via hand-rolled `format!()` (pinned: matches POC, no new dep; `AgentLabel::parse` already restricts charset so no escape-safety issues to manage). Match the POC's shape verbatim:
   - Wrapper: `zellij:tab-bar` plugin (size=1) on top + `zellij:status-bar` plugin (size=2) on bottom.
   - Middle: `pane split_direction="horizontal" { ... }` containing:
     - Master pane: `name="<label> (master)"`, `command "clank"`, `args "agent" "start" "<label>"`.
     - Reviewer panes (one per reviewer, in declaration order): `name="<label> (reviewer)"`, same command shape.
   - Per-pane command is `clank agent start <label>` — assumes the agent is already bound. If not, `clank agent start` errors with a clear "run `clank as <label>` first" message (already shipped in `agent-config-and-start`).
   - **Why not bare tool + seed prompt like the POC?** The POC opens a FRESH worktree where agents haven't bound yet, so it needs the self-bind seed. `clank open zellij` operates on the current repo where agents are typically already bound. Users wanting fresh-worktree spawning can keep using `open-worktree.sh` (or queue a follow-up plan).
3. **Spawn the tab.** Shell out to `zellij action new-tab --cwd <repo-root> --name <basename> --layout-string <KDL>`. Tab name = repo basename (matches POC; no flag needed in v1).
4. **`--print` flag** (mirrors `clank agent start --print` and `clank diff --print`): emit the composed KDL on **stdout**, emit the would-be-spawned argv on **stderr** as a single line (e.g. `spawn: zellij action new-tab --cwd /path/to/repo --name basename --layout-string <KDL>`), exit 0, don't shell out. Ruthless 7a58d12 pinned: mirrors `clank diff --print`'s "argv on stdout + env on stderr" convention. Tab name + cwd are part of the spawn metadata; surface them the same way so test 8 can assert in `--print` mode without mocking zellij.
5. **Sanity check**: if `ZELLIJ_SESSION_NAME` env var is unset AND `--print` is not passed, error with "not inside a zellij session." Matches POC.

### CLI shape

Add to `crates/cli/src/cli/mod.rs`:

```rust
#[derive(Args, Debug)]
pub struct OpenArgs {
    #[command(subcommand)]
    pub command: OpenCmd,
    /// Repo root override.
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(clap::Subcommand, Debug)]
pub enum OpenCmd {
    /// Classify a path against the active repo; emit JSON or human-readable.
    Dry(OpenDryArgs),
    /// Auto-generate a zellij layout (KDL) for master + reviewer panes.
    Zellij(OpenZellijArgs),
}

#[derive(Args, Debug)]
pub struct OpenDryArgs {
    pub path: String,
    #[arg(short = 'j', long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct OpenZellijArgs {
    /// Emit the composed KDL on stdout and exit 0 without spawning zellij.
    #[arg(long)]
    pub print: bool,
}
```

`main.rs` dispatches `Command::Open(OpenArgs)` → match on `args.command`.

### Migration (in-scope)

Verified migration sites (homework done above):

- **`crates/cli/src/cli/open.rs`** — refactor module to expose `pub async fn run_dry(args: OpenDryArgs)` and `pub async fn run_zellij(args: OpenZellijArgs)`. Keep the path-classifier logic intact; just shift the entry point.
- **`crates/cli/tests/open_integration.rs`** — ~5 test invocations need `clank open dry` instead of `clank open`.
- **`open-worktree.sh` POC** — update `clank open --json "$REPO_ROOT"` → `clank open dry --json "$REPO_ROOT"`. (Untracked but shipped to lloyd; worth keeping aligned.)
- **`crates/cli/src/cli/setup_assets/*.md`** — verified absent. No changes.
- **Codex allow-list** — bare `["clank"]` pattern covers any subcommand. No changes.

The breaking change is small; clap will give a clear "unrecognized subcommand" error if anyone misses a site.

## Out of scope

- Per-agent pane sizing customization. Default reasonable shape; iterate if users want.
- Other multiplexers (tmux, screen, kitty splits). One layout target per plan; tmux can be a follow-up.
- Subcommand discovery of additional `clank open` siblings beyond `dry` and `zellij`.

## Acceptance

- `clank open dry <path> [-j]` behaves identically to today's `clank open <path>` (same JSON shape, same human output, same error semantics).
- `clank open <path>` errors with a clap "unrecognized subcommand" message; existing `clank open --json <path>` callers see the failure immediately at the call site.
- `clank open zellij` in a repo with one master + N reviewers (per `load_merged_agents`) opens a new zellij tab named after the repo basename. Master pane on top + N reviewer panes stacked below, each running `clank agent start <label>`. Wrapper plugins (`zellij:tab-bar`, `zellij:status-bar`) match the POC.
- `clank open zellij --print` emits the composed KDL on stdout, exits 0, does NOT shell out to zellij.
- Repos with no master agent error: "no master agent registered for this repo — register one with `clank agent add <label> --role master`."
- `clank open zellij` (without `--print`) errors when `ZELLIJ_SESSION_NAME` is unset: "not inside a zellij session — run from a shell pane inside zellij, or pass `--print` to emit the KDL only."
- Unit tests cover KDL generation against fixture agent sets; integration tests use `--print` mode (no real zellij needed in CI).
- `cargo test --workspace` passes.

## Tests

Per the established pattern, named tests in the integration file (`crates/cli/tests/open_zellij_integration.rs` + migration of existing `open_integration.rs`):

1. `open_dry_emits_same_shape_as_legacy`: existing `open_integration.rs` tests updated to `clank open dry`; outputs identical. Plus a `legacy_open_invocation_errors_with_dry_hint`: assert `clank open <path>` (without the `dry` subcommand) fails AND the error mentions `dry` (phrasing-agnostic — clap's exact wording is whatever it is, but the missing-subcommand should surface).
2. `open_zellij_print_emits_kdl_with_master_and_reviewers`: 1 master + 2 reviewers (typed `RepoConfigFile` + `DefaultAgent` setup per `typed-config-dogfood`); assert KDL contains `name="<m> (master)"` and `name="<r1> (reviewer)"`, `name="<r2> (reviewer)"`, each `command "clank"` + `args "agent" "start" "<label>"`.
3. `open_zellij_print_includes_tab_bar_and_status_bar_plugins`: assert `plugin location="zellij:tab-bar"` and `plugin location="zellij:status-bar"` appear in the composed KDL.
4. `open_zellij_no_master_errors_with_suggestion`: 0 masters + 2 reviewers; assert error names `clank agent add` and `--role master`.
5. `open_zellij_multiple_masters_errors_with_diagnostic` (new per ruthless 7a58d12): 2 masters + 1 reviewer; assert error names BOTH master labels AND mentions `clank agent set-role`. Fail-loud lockdown.
6. `open_zellij_no_zellij_session_errors_without_print`: `ZELLIJ_SESSION_NAME` unset, no `--print` → error names the env var and the `--print` escape.
7. `open_zellij_no_zellij_session_succeeds_with_print`: same as #6 but `--print` is passed → succeeds and emits KDL.
8. `open_zellij_reviewer_order_matches_declaration_order`: write a `RepoConfigFile` with `agents` listed in explicit order `[master, bob, alice, codex]`; assert KDL panes appear in same order (`master / bob / alice / codex`). Per ruthless 7a58d12: explicit order setup so the test doesn't pass by accident on a BTreeMap-ordered declaration.
9. `open_zellij_tab_name_in_print_spawn_metadata` (per ruthless 7a58d12 pinning option a): temp repo at a known basename `weirdname`; `--print` → assert stderr `spawn:` line contains `--name weirdname` and `--cwd <repo-root>`. Tests the tab name without needing to mock zellij or check actual spawn.

## Dependencies (all FINISHED)

- `agent-config-and-start` (FINISHED `18a5e78`): shipped `clank agent start <name>`. The per-pane launch command.
- `agent-add-cli-and-repo-scope` (FINISHED `1cf029b`): shipped `load_merged_agents` + declaration-as-source-of-truth. The agent enumeration source.

## Related history

- `open-worktree.sh` (untracked POC): functioning prototype. This plan ships the layout part as `clank open zellij`; the worktree creation + clank init remain in the POC for users wanting the bundled flow.
- `worktree-workflow-research.md` (stub): broader research on multi-agent worktree flows. Different scope.
