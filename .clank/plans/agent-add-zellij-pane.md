# agent-add-zellij-pane

`clank agent add`/`remove` should keep the live zellij reviewer stack in
sync with the roster: adding a reviewer while inside a zellij session
opens its pane in the stack; removing one closes that pane.

## Core model

The roster (`.clank/config.json`) is the source of truth. A zellij pane
is a *projection* of a roster reviewer onto the live session. `agent
add`/`remove` already mutate the roster; this plan makes them, as a
**best-effort side-effect**, reconcile the current tab's reviewer stack
to match — spawn the missing pane on add, close the stale pane on
remove. The roster mutation persists FIRST and is the success criterion;
a zellij failure must never fail (or roll back) the add/remove.

### Single source of truth (do this first — architecture)

**`clank agent start` (agent.rs) is UNCHANGED — it is NOT made
zellij-aware.** It already does the right thing: launch the agent's tool
in whatever pane it runs in. All zellij behavior lives in `agent
add`/`remove`. The new pane simply *runs* `clank agent start` as its
command, exactly as the launch-time layout already does.

The only shared concern is the *argv that invokes* `agent start`. The
launch-time layout encodes it as a KDL string in `push_agent_pane`
(open_zellij.rs:765): `command "clank"` / `args "agent" "start"
"<label>" "--repo" "<repo>"`, with the pane name from
`agent_pane_title(label, role)` (open_zellij.rs:761). The live
`new-pane` path needs the SAME argv (as a `Vec<String>`, not KDL) and
the SAME title, or layout-spawned and live-spawned panes drift and
`parse_agent_panes` (status_tui.rs:1019) stops matching one of them.

Factor that invocation argv into one helper (e.g. `agent_start_argv(label,
repo) -> Vec<String>`) **in open_zellij.rs** and have BOTH
`push_agent_pane` and the new live path build from it. This touches the
layout builder only — not `agent start` itself. Title stays centralized
in `agent_pane_title`. No new copy of either format.

## Behavior

**add (reviewer, repo scope):** after a successful `add_repo_roster_agent`
/ `add_repo_roster_agent_by_name`, if `$ZELLIJ` is set:
- Idempotency: if a pane titled `<label> (reviewer)` already exists in
  the current tab (`parse_agent_panes` over `zellij action list-panes`),
  do nothing.
- Otherwise spawn it into the reviewer stack running `clank agent start
  <label> --repo <repo>`, named `agent_pane_title(label, "reviewer")` so
  `remove`/the TUI can find it. `agent start` already handles unbound
  agents (bootstraps a fresh tool session — agent.rs:221), so a
  brand-new agent's pane boots cleanly.

**remove (reviewer, repo scope):** after a successful `remove_repo_agent`,
if `$ZELLIJ` is set, find the pane for `<label>` via `parse_agent_panes`
and close it by id. No pane found → no-op.

## Scope / non-goals

- **Reviewers only** (roles `commit` + `gate`). Master is explicitly out
  of scope for now — do not spawn/close the master/stage pane.
- **Repo scope only.** `--global` add/remove (the user-scope definition
  library) must NOT touch zellij.
- **`clank agent start` is unchanged** — not zellij-aware. The feature
  lives entirely in `add`/`remove`; `start` is only invoked as the new
  pane's command.
- Both add modes (by-name copy-down and `--tool` inline) fire.
- Non-zellij invocations and any `zellij` failure are silent no-ops
  (mirror the fail-soft `.output()`-discard pattern used by the
  status_tui zellij helpers — a hiccup must not bleed onto a pane or
  fail the command).
- Target the CURRENT tab's stack only (scope via `current-tab-info`,
  like status_tui), not panes in other repos' tabs.

## Integration points

- `crates/cli/src/cli/agent.rs`: `add` (554) / `remove` (745) dispatch;
  hook after the repo-scope reviewer mutation succeeds. Gate on role ∈
  {Commit, Gate} and on NOT `--global`.
- Reuse: `agent_pane_title` + factored `agent_start_argv`
  (open_zellij.rs), `parse_agent_panes` + `zellij_list_panes` +
  `zellij_current_tab` (status_tui.rs). Promote to `pub(crate)` as
  needed; keep them the only definitions.
- `$ZELLIJ` detection: `std::env::var_os("ZELLIJ").is_some()` (as in
  open_zellij.rs:16, fork.rs:87).

## Implementation questions to resolve empirically (zellij ≥0.44.3)

1. **Land a new pane IN the stack.** `new-pane` opens relative to focus.
   Likely: look up an existing reviewer pane id (`parse_agent_panes`),
   `focus-pane-with-id`, then `new-pane --name "<title>" --cwd <repo> --
   clank agent start <label> --repo <repo>` so it joins the stack.
   Decide the empty-stack case (no existing reviewer pane to join):
   acceptable to open a plain tiled pane, or document the limitation.
   Restore focus afterward if practical.
2. **Close a specific pane.** Confirm whether `close-pane` can target a
   pane id directly; if not, `focus-pane-with-id <id>` then `close-pane`.
   Verify against the actual zellij action surface.

## Testing (respect no-binary-spawning-tests)

No spawning of zellij or the clank binary. Unit-test the pure pieces in
process: `agent_start_argv` composition; the `new-pane` argv builder
(name/cwd/command); and pane selection — given captured `list-panes`
output, the right pane id is chosen for a given reviewer (reuse
`parse_agent_panes`). The thin `Command::new("zellij")` wrappers stay
untested like the existing status_tui ones.

## Acceptance

- In a zellij `clank open` session: `clank agent add <reviewer>` makes a
  new reviewer pane appear in the stack running that agent; `clank agent
  remove <reviewer>` closes it.
- Outside zellij / on zellij error: add/remove behave exactly as today
  (roster mutated, no error).
- `--global` and master operations unchanged.
- No duplicated pane-title/launch-argv format; layout and live panes
  share one source.
