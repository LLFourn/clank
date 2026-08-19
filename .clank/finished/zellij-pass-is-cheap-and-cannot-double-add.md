# zellij-pass-is-cheap-and-cannot-double-add

## Priority one: never call a zellij action that enumerates processes

`zellij action list-clients` costs ~1 SECOND per call on this machine.
clank calls it once per reconcile pass, to learn ONE integer: which
pane has focus, so focus can be restored afterwards.
`pass_focus_target()` → `client_focused_pane()` → `list-clients`
(`open_zellij.rs:1004`).

### Root cause, established from zellij's source and measured

`list-clients` routes through `populate_session_layout_metadata`
(`zellij-server/src/pty.rs:1943`) — the same path `dump-layout` uses,
which is why that is also slow. For every pane it resolves the child
process's cwd and command line:

- `get_cwds(pids)` — sysinfo with `with_cwd(UpdateKind::Always)`
- `get_all_cmds_by_ppid(..)` — **shells out to `ps -ao ppid,args`**

Measured here: `ps -ao ppid,args` alone takes **1071 ms** with 1308
processes on the machine. `list-clients` on an affected session
measured 1041/1036/1035/1038/1034/1039 ms — held to ±3 ms, so this is
one subprocess enumerating every process on the machine, NOT work
proportional to the session.

Two consequences that make this worse than a constant:

- The cost scales with processes on the MACHINE, not panes in the
  session. Every agent added slows every pass in every session.
- Each zellij server runs the binary it started with. Sessions started
  before the current binary (installed 2026-07-17) pay it; ones
  started after do not — measured 946/1036/668 ms for sessions up
  61d/44d/40d against 34–41 ms for 1d/5d/7d/8d/12d/26d. A user-visible
  workaround (restart old sessions) exists, but clank must not depend
  on it.

### The replacement, fully determined by measurement

- `list-panes --json` is ~30 ms and FLAT across session sizes. The
  pass already fetches it.
- `is_focused` alone is ambiguous, exactly as the existing comment
  says: measured 13 focused panes across 13 tabs — one per tab.
- `current-tab-info` names the active tab (`id:`) and costs **34 ms
  even on an affected server**, because it does not touch
  `populate_session_layout_metadata`.

So the focus target is: the pane with `is_focused` whose `tab_id`
equals `current-tab-info`'s id. One cheap call replaces a one-second
one; `ZellijPane` needs `is_focused` added to its deserialized subset.

Keep the existing fail-soft behaviour: when the target cannot be
determined, restore NOTHING rather than guess (codex 5d498d0).

### The rule this plan establishes

No clank code path may call a zellij action that goes through
`populate_session_layout_metadata` — today `list-clients` and
`dump-layout`. Enforce it the way the git boundary is enforced: a test
that fails the build when production code names one.

## Then: make a double-add impossible, not merely unlikely

Two panes for one agent. Observed live: a tab with TWO
`codex (reviewer)` panes, the second dead on arrival —

    Error: Failed to resume session from …rollout-2026-08-17….jsonl:
    thread/resume failed: thread … already has an active writer
    (code -32600)   [ EXIT CODE: 1 ]

A duplicate is not cosmetic: the tools refuse to open one session
twice, so the second pane is a corpse the reconciler still counts as a
live pane for that label.

Cutting the pass from ~1 s to ~60 ms shrinks the race window; it does
not close it. `add_reviewer_pane`'s idempotence guard reads the
PASS-START snapshot, so a pass beginning before a previous pass's pane
appears in a listing cannot see it and creates a second one.

### First question: HOW MANY drivers made the duplicate

This decides everything else, so it is answered before any mechanism
is chosen.

The tree already names the multi-driver cause: `PanePlan::remove`'s
own doc says "a two-TUI race can double-open" (codex c7be87f), and
the excess copy is closed by convergence rather than prevented. It is
not hypothetical here — measured on this machine, the repo
`frostsnap/.clank/worktrees/full-app-sim-driver` has TWO live
`clank status --tui` processes.

That matters because every single-process remedy fails against two
drivers: listing-then-create across two processes is a lock-free
TOCTOU, and any "remember what I created" memory is per-process. So:

- If ONE driver made it, a within-process fix suffices — either the
  guard consults a listing taken AFTER the previous pass's creations,
  or the reconciler remembers its creations until a listing confirms
  them. Establish why the worker's coalescing
  (`worker_coalesces_in_flight_arrivals_into_exactly_one_follow_up`)
  did not already serialise them.
- If TWO drivers made it, "impossible" requires a cross-process
  mechanism (a repo-scoped lock around the create, or serialisation on
  zellij's side).

### Answered: TWO drivers, so the fix is cross-process

A single driver cannot double-add. Its worker is ONE thread draining
an in-process `mpsc::Receiver` — "one worker serializes passes by
construction" — and `reconcile` has exactly one production caller, on
that thread. So the observed duplicate came from two drivers, and
every within-process remedy is beside the point.

The fix is therefore a repo-scoped RECONCILE LEASE, the same shape as
the ingest lease (wal-single-ingest-writer): exclusive, non-blocking
flock on `<repo>/.clank/zellij-reconcile.lock`, held for as long as a
process drives the repo and re-attempted while absent so closing the
holder hands over rather than stranding. The loser caches nothing — the
holder's convergence is not its to claim, and if the lease frees it
must act on whatever was left.

The lease is scoped to RECONCILE — structural pane work and
convergence caching — and NOT to retitles, which stay dual on
purpose: pane titles are session-local, so a loser driving a different
zellij session never receives the holder's stamps and would sit
permanently unglyphed if gated. Within one session both write the same
title from the same inputs.

Non-blocking rather than queued is deliberate: a queued pass would
apply a plan computed against a roster the holder has already changed.

This makes the double-add impossible rather than unlikely, which is
what the strand's title promises. Multi-TUI itself stays supported —
the tree already anticipates it, noting that "another TUI may already
have converged the live layout" (codex afb6d43); what was missing was
that both could ACT.

## Do not let a dead pane read as a live agent

The duplicate exited (code 1) and stays in the listing.

**Decided: an exited pane DOES count as its label's pane**, which is
also what the code does today — no reader consults exit state. Respawn
is MANUAL by design (the exit indicator, then an explicit re-run);
auto-respawn would relaunch a tool that just failed, and for one that
refuses a second session it would relaunch it straight back into
`already has an active writer` on every pass. The stated cost is
accepted: a crashed agent blocks its own replacement until someone
acts.

**One rule, shared by every reader.** The decision cannot be made for
the idempotence guard alone: `plan_panes` counts panes per label for
multiplicity, and `remove_target_ids` walks panes in LISTING order,
taking the first match within budget with no preference at all.

So a guard-only rule is actively worse than none. The guard skips the
corpse and creates a fresh pane; the count still sees the corpse and
reads 2; the dedup closes the FIRST match in listing order — which can
be the live replacement — and the corpse survives to repeat the whole
cycle.

Both halves are pinned: the rule is stated once, on the classifier
both readers go through (`agent_pane_label`), and
`remove_target_ids` closes exited copies FIRST. The preference decides
only WHICH pane closes, never how many — a label leaving the roster
still loses all of its panes.

## Required tests

- The pass derives its focus target without `list-clients`, from
  `is_focused` + the active tab, and still restores nothing when the
  target cannot be determined.
- A build-failing check that no production path names `list-clients`
  or `dump-layout`.
- A driver without the lease performs no STRUCTURAL pane work and
  caches no convergence.
- A driver without the lease still retitles — the session-local
  exception, asserted so it reads as intent rather than a gap.
- The lease admits exactly one holder, refuses the second rather than
  queuing, and frees on drop so reconciliation hands over.
- An exited pane is treated per the decision above, asserted either
  way.
- `remove_target_ids` closes the EXITED copy, not the live one, when
  a label has both.
- No test spawns zellij or an agent binary.

## Out of scope

- Upstream zellij's `ps` call. Worth reporting; this plan routes
  around it rather than waiting.
