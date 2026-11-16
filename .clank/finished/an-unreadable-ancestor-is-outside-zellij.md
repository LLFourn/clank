# an-unreadable-ancestor-is-outside-zellij

Invariants: I6, I11, I12.

## Why

`zellij_endpoint::endpoint_of_with` walks a process's ancestors to its zellij server. In a fresh Ghostty window the walk reaches root-owned `login`. `proc_pidinfo(PROC_PIDTBSDINFO)` refuses that with EPERM, so the walk returns `Unknown` and `clank open` refused.

- e3c598b2 fixed `open`, by the user's rule that an unreadable ancestry must not prevent normal opening, even with `ZELLIJ` set ("It should just fallback to opening normally"). That stays.
- The other callers of `own_endpoint` still fail the same way in a shell outside zellij: `caller_identity_from`, `workspace_session` (used by web) and the TUI `ReconcileWorker`.
- 9af10ef3 added `Report::SessionFound(bool)` beside `ZellijReach`. These are two facts: a session was found, and its pane listing answered. The TUI seeds the first as `false` before any report, so at TUI start an agent page says "no Zellij session found" before the first listing.

EPERM is a permission outcome, not evidence of who owns a process, so it cannot prove a walk has left zellij. But a process zellij spawned into a pane inherits `ZELLIJ` from it. A process with `ZELLIJ` unset has no pane in its ancestry, except one whose parent scrubbed its environment, which clank does not do. This uses the environment only to ask in or out, never which session: I12 stands.

## Build

1. `own_endpoint`: when the walk is `Unknown` and this process's `ZELLIJ` is unset, the answer is `NotInZellij`. With `ZELLIJ` set, `Unknown` stays `Unknown`. `endpoint_of(pid)` for another process is unchanged, since this process's environment says nothing about it. No new `unsafe` and no ownership inference from errno.
2. `open` keeps e3c598b2's fallback. Its special case becomes redundant where `ZELLIJ` is unset, and stays the policy where it is set.
3. Keep `Report::SessionFound` as its own fact, but make it unknown until the first report arrives (`Option<bool>` or an equivalent three-state). A page claims "no Zellij session found" only on a report saying so. The existing failed-listing and reopen coverage from `reopen-is-available-when-the-session-is-found` stays: reopen is offered whenever the session is found, even when the listing failed.
4. Registry:
   - I11, narrowed to what this proves: `clank open` works when no zellij session exists. It becomes `holds`, naming the tests below.
   - Drop "never nests one", since an unreadable ancestry inside a pane still falls back by the user's rule. Say so in `Scope`.
   - Drop this plan from I6's `Plans`, and remove the reopen item from I6's `Today`: those are two facts, not a duplicate.
   - I12 keeps its tests.

## Acceptance

- Resolver tests through the `Processes` seam and an injected environment:
  - `Unknown` with `ZELLIJ` unset is `NotInZellij` for `own_endpoint`'s rule;
  - `Unknown` with it set stays `Unknown`;
  - a readable chain with a server is still `Found` whatever the environment says.
- Each listed caller outside zellij, with an unreadable ancestor and `ZELLIJ` unset, proceeds as not in zellij.
- TUI: before the first report, an agent page does not say no session was found. The existing failed-listing and reopen tests still pass unchanged.
- The existing `denied_or_raced_inspection_is_unknown_never_not_in_zellij` stays as is: denial alone is still `Unknown`.
