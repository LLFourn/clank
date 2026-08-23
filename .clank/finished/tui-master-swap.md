# tui-master-swap

## Why

`swap-is-a-tui-action-too` added the roster swap picker to reviewer detail
pages but deliberately excluded the master. That exclusion is now the bug:
the status TUI cannot replace the current master with an agent from the
user-scope library.

The refusal exists at both layers:

- `detail_actions(RosterRole::Master)` omits `DetailAction::Swap`.
- `swap_repo_agent` rejects an outgoing master and redirects the user to
  `agent promote`, even though promotion requires the replacement to be
  already on the roster and therefore cannot implement an atomic swap with
  an off-roster library agent.

## Behavior

The master's detail page offers `swap for another` and opens the existing
swap candidate screen. Its candidates remain the user-scope agent library
minus the current roster. Choosing one atomically removes the outgoing
master and inserts the selected agent with `RosterRole::Master`.

This is replacement, not promotion:

- Swapping the master with an off-roster library agent removes the outgoing
  master from the roster.
- Making an existing reviewer the master remains `PromoteToMaster`; that
  demotes the old master to `Commit` and keeps both agents on the roster.

The distinction must be visible in code and tests so the two operations do
not drift into one ambiguous picker.

Once the config write removes the outgoing master, that identity is no
longer a participant in this repo. It must resolve to an explicit "not on
the roster" error, never fall through to `Role::default()` and silently
become a reviewer. A wait already parked under the outgoing identity must
terminate with that diagnostic on the roster-change wake. The stop hook must
likewise arm no replacement wait and explain that the bound identity is no
longer registered.

This invalidation is distinct from a transient config read failure. Parked
waits retain their existing fail-soft retry for transient failures, but
"resolved roster successfully and this label is absent" is durable identity
revocation and must not retain the last-known master role.

The current master may initiate this swap from its own TUI pane. Existing
reconcile ordering is load-bearing: add the incoming master pane, relocate
it into the master slot, then remove the outgoing pane. Preserve and test
that ordering so the operation does not close the initiating pane before a
replacement exists.

## Implementation

Generalize `swap_repo_agent` to carry every outgoing role, including
`Master`, across in the same single config write it already uses for
reviewers. Keep all existing validation: `<out>` must be on the roster,
`<in>` must not already be on it, and `<in>` must resolve from the
user-scope library.

Make role derivation total over roster membership rather than defaulting an
unknown label to `Reviewer`. The pure registered-set resolver should expose
absence explicitly (for example `Option<Role>`), and the filesystem-facing
resolver should return a typed or otherwise structurally distinguishable
"agent is not on this repo's roster" error. Update role consumers rather
than string-matching diagnostics.

At wait arm time, absence remains a hard error. During a parked wait's
per-wake re-derivation, terminate on the explicit not-registered outcome;
retain the existing last-known-good inputs only for transient failures. The
stop-hook path should use the same resolver and therefore emit its existing
diagnostic/no-arm outcome for the removed identity.

Expose `DetailAction::Swap` on the master's detail page and reuse the
existing `SwapPicker`, available-agent source, refresh rebinding, rendering,
and `apply_swap` path. Do not invent a second candidate model.

The operation remains config-only. It must not call zellij or launch an
agent. The status TUI reconciler observes the one roster transition and owns
opening/removing/retitling/relocating panes, including the already-supported
arrival of a brand-new master.

## Required tests

- `detail_actions(RosterRole::Master)` includes `ToggleAuto`, `Swap`, and
  `Back`; reviewer action sets remain unchanged.
- `swap_repo_agent(master, off_roster_agent)` succeeds in one persisted
  transition: the outgoing label is absent, the incoming description is
  copied from the library, exactly one master exists, and the incoming label
  owns that role.
- The CLI `clank agent swap <master> <incoming>` follows the same behavior;
  remove the obsolete error that redirects master swaps to promotion.
- Choosing a candidate from the master's TUI swap picker returns to the
  agent panel and the refreshed roster contains the replacement master.
- Cancelling the picker leaves config byte-identical; a failed swap remains
  on the master detail page and surfaces the reason.
- A reviewer swap still carries its exact review tier and still requires a
  fresh review as before.
- A master swap does not alter the active plan's expected reviewer tiers or
  rewind a settled reviewer gate merely because the non-reviewing master
  identity changed.
- An unknown label in a successfully resolved roster has no role; it cannot
  default to `Reviewer`. Pin the pure resolver and the public role-resolution
  diagnostic.
- An in-process wait parked as the outgoing master wakes on the swap, exits
  with an actionable "not on this repo's roster" diagnostic, and emits no
  reviewer work. This test must drive the real per-wake re-derivation path,
  not merely call the resolver. A transient read failure remains fail-soft
  and retries as before.
- The stop-hook decision for the swapped-out, auto-enabled identity is
  diagnostic/no-arm, not a reviewer wait.
- The action performs no pane or process work. Exercise the reconciler seam
  with fake pane data to prove the roster transition is the only input, a
  newly arriving master converges through the existing path, and operation
  order is add incoming -> relocate incoming -> remove outgoing.
- No test spawns zellij or an agent binary.

## Out of scope

- Combining promotion candidates and off-roster swap candidates into one
  screen.
- Changing promotion semantics or reviewer demotion tier (`Commit`).
- Adding swap-specific pane orchestration.
