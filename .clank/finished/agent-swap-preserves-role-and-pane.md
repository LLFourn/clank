# agent-swap-preserves-role-and-pane

## Problem

Replacing one agent with another is two operations today: remove the
old, add the new. That is wrong in three ways.

- **The roster has a hole in between.** A pass that observes the
  middle state sees a role with no agent. For a `master` swap that is
  a repo with no master; for a reviewer it is a gate whose expected
  set just shrank, which can let a plan finalize on fewer reviews than
  intended.
- **The role is re-entered by hand.** Nothing ties the incoming agent
  to the outgoing one's role, so a swap silently becomes a demotion or
  promotion on a typo.
- **The pane moves.** Remove closes the pane, add opens a new one
  wherever `new-pane` finds space. zellij 0.45.0 has no action that
  moves a pane between tabs (`BreakPane*` are keybindings only), so a
  new pane that lands in the wrong tab is stranded until someone
  closes and respawns it by hand.

`agent set-review` already establishes the principle for the tier
case — "no remove/re-add, so the agent's session and zellij pane are
undisturbed". Swap is the same principle applied to the identity.

## Approach

`clank agent swap <out> <in>` — one command, one config write, and NO
zellij work in the command at all.

1. **Atomic in the roster.** The declaration for `<out>` is REPLACED
   by `<in>` in a single `config.json` write, carrying the role across
   verbatim. No intermediate state in which the role is vacant.

2. **Pane lifecycle stays with its existing owner.** Every roster
   command is already pure config — `add` ("No zellij projection here:
   the status TUI owns roster→pane convergence"), `promote` ("No
   zellij relocation here"), `remove` ("Pane closure moved to the
   TUI's roster→pane reconciler"). Swap is not an exception, and an
   earlier draft of this plan made it one.

   That draft had swap create the incoming pane, stack it, write the
   config, then close the outgoing pane. It produced a race on its own
   — between create and write the incoming pane is roster-absent, and
   `plan_panes` removes every live pane whose label the roster does not
   contain, so the reconciler destroys the pane swap just made. The
   proposed remedy was a pre-close re-check plus a "could not preserve
   the position" report. Both were patches over a second owner of pane
   lifecycle. With one owner the window does not exist, and neither
   does the reporting path.

3. **Position preservation belongs in the reconciler, and is not
   swap-specific.** A swap reaches the reconciler as what it is: one
   reviewer leaving, one arriving, in a single pass. `plan_panes`
   already computes both sets together and already runs removes LAST
   ("its pane lives until the removes, which run after the layout").

   What is missing is only that a departing label is not offered as an
   anchor: `add_reviewer_pane` anchors on `other_reviewers`, which is
   the NEW roster, so the pane about to be vacated is invisible to it
   and the arriving pane opens wherever `new-pane` finds space. Offer
   the departing labels as anchor candidates too. The arriving pane
   then opens in the right tab and joins the right stack, and the
   removes that follow leave it in place.

   This is strictly more general than swap: a manual `agent remove x`
   followed by `agent add y` in one pass gets the same preservation,
   with no swap-specific signal for the reconciler to interpret.

4. **When no departing pane can be found**, placement falls back to
   what it does today. No new failure mode and nothing to report — the
   command never claimed to place panes.

## Settled decisions

- **Naming: `clank agent swap <out> <in>`.** The `fork` trap came
  from `fork` taking a FREE POSITIONAL name, so `clank fork list`
  created a fork called `list`. `agent` is subcommand-nouned
  (`add` / `remove` / `promote` / `list` / `set-review` / `start`), so
  a positional is only ever read after an explicit subcommand and an
  agent literally named `swap` stays reachable through all of them.
  The trap cannot recur here.

- **Both labels are validated before anything is written.** `<out>`
  must be on the roster; `<in>` must NOT be. A pure-config command's
  validity conditions are its whole contract, so neither is left
  implicit.

  `agents` is a map keyed by label, so a swap onto a label already
  present does not duplicate it — it COLLAPSES two entries into one.
  `<out>` is removed, `<in>`'s existing declaration is overwritten
  with `<out>`'s role, and the roster silently shrinks by one while
  `<in>`'s review tier changes underneath it. That shrinks the gate's
  expected reviewer set, which is the exact hazard named at the top of
  this plan. Refuse, pointing at `agent set-review` for a tier change
  and `agent remove` if the caller really means to drop one.

- **The master role is refused, pointing at `agent promote`.** That
  command already demotes the previous master and `relocate_for_promote`
  already owns the pane relocation. Duplicating it would give two
  code paths for one transition.

- **The departed agent's pending verdict is VOID.** If `<out>` has
  feedback at the current sha, `<in>` owes a fresh review. A verdict
  is an agent's judgement, not the role's — inheriting it would let an
  agent that never read the code hold an approval. This can re-open a
  gate that was about to close, which is the correct outcome.

- **`.clank/agents/<out>/` is left in place**, so swapping back
  resumes rather than starting cold. This is what `agent remove`
  already does ("per-agent skeleton dir + feedback history are
  preserved"), and nothing in `agent.rs`, `agent_store.rs`, or
  `doctor.rs` collects a roster-absent dir.

## Required tests

- The roster is never observed with the role vacant: the config write
  is one operation, and a failed swap leaves the original roster byte
  for byte.
- The incoming agent holds exactly the outgoing agent's role, for
  every reviewer tier in the vocabulary.
- `agent swap` performs NO zellij calls — asserted, since the whole
  design rests on it.
- In a pass that both adds and removes reviewers, the arriving pane is
  anchored on a departing pane: same tab, same stack.
- The preservation is not swap-specific: a plan built from a separate
  remove and add gets it too.
- With no departing pane, placement is unchanged from today.
- Removes still run after the layout, so the arriving pane survives
  the departure.
- Swapping the master is refused, and the error names `agent promote`.
- A swap whose `<out>` is not on the roster is refused, and nothing is
  written.
- A swap whose `<in>` is ALREADY on the roster is refused, and nothing
  is written — asserted on the roster afterwards, since the failure
  mode is a silent one-entry shrink rather than an error.
- The gate after a swap requires a fresh review from `<in>`: a
  verdict `<out>` left at the current sha does not satisfy it.
- No test spawns a real agent binary.

## Out of scope

- Swapping across repos.
- Changing what a role means, or the gate arithmetic itself.
