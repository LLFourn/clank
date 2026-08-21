# swap-is-a-tui-action-too

## Why

`clank agent swap <out> <in>` exists and works, but the status TUI's
agent detail page does not offer it. `DetailAction`
(`status_tui/input.rs:138`) gives a reviewer:

    ToggleAuto · TierCommit · TierPlan · TierFinal · PromoteToMaster · Remove · Back

So the page covers `agent set-review` (the tier checkboxes), `agent
promote`, and `agent remove`. Swap is the ONE roster mutation with a
CLI command and no counterpart on the page a human actually uses to
manage the roster — you can promote an agent or delete it from the
team, but replacing one means dropping to a shell.

## It must stay a config write

The reconciler owns roster→pane convergence. `swap_repo_agent`'s own
doc already states the rule and the reason:

    Pure config, like every sibling roster command — the status TUI
    owns roster→pane convergence (tui-zellij-pane-reconcile). Giving
    this command its own pane orchestration would make it a second
    owner, and a pane it created before the write would be
    roster-absent, which is exactly what the reconciler destroys.

So the action does what `PromoteToMaster` does — call the core, write
config, return to the panel — and NOTHING else:

    DetailAction::PromoteToMaster => match agent::set_repo_master(repo, &label) { … }

Do not call zellij. Do not open, close, retitle, or move a pane. The
config write is the whole action; the watcher observes the roster
change on its next snapshot and converges the panes. Anything else
makes a second pane owner, which is the bug the reconciler exists to
prevent and which it will actively undo.

`swap_repo_agent(repo, home, out, into)` is already pure config and
already carries the role across in ONE write, so the role is never
observed vacant. Reuse it as-is; this plan adds no core logic.

## The one genuinely new thing: swap needs a second operand

Every existing `DetailAction` acts on the agent whose page you are on.
Swap needs an INCOMING label too, which the CLI takes as an argument
and the TUI has to ask for.

That picker already exists — the "+ add agent" flow offers
`available_agents` (`status.rs:230`), the user-scope library minus the
current roster, which is exactly the right candidate set for a swap.
Reuse it rather than growing a second picker; a second list of "agents
you could add" would drift from the first.

Selecting a candidate performs the swap; backing out performs nothing.

## Refusals

The core already refuses to swap the master (`agent promote` is that
operation) and refuses an incoming label already on the roster. Follow
the page's existing defense-in-depth convention: the master's reduced
action set hides what it cannot do, AND the core refuses anyway. So
hide swap on the master's page rather than offering an action that
always errors, and keep the core's refusal.

## Required tests

- Swap appears in `detail_actions` for every reviewer tier and is
  ABSENT for `RosterRole::Master`.
- Choosing a candidate calls the swap core and lands back on the agent
  panel, with the roster showing the incoming label at the outgoing
  agent's role.
- A failing swap keeps the user on the detail page and surfaces the
  error, matching `PromoteToMaster`'s handling — a silently unchanged
  roster reads as "clank ignored me".
- Backing out of the picker leaves the roster byte-identical.
- **The action performs NO pane work.** Assert against the same seam
  the reconciler tests use, so a future implementation that "helpfully"
  opens the incoming agent's pane fails here.
- No test spawns zellij or an agent binary.

## Out of scope

- Changing `swap_repo_agent` itself. It is correct, tested, and shipped
  (`ee4901a`); this plan only reaches it from a second surface.
- Any other missing TUI action. Swap is the identified gap; a general
  audit of CLI-vs-TUI parity is its own piece of work.
