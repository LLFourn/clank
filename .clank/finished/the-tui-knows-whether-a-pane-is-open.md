# the-tui-knows-whether-a-pane-is-open

The agent row's mark is `▶` or `⏸` — the auto-mode — and it reads as
"running". It is not. An agent whose pane was closed by hand, or
whose process exited and left zellij's `EXITED` corpse, shows the
same green triangle as one that is working. And the `reopen pane`
item sits on every agent's page whether or not there is anything to
reopen, because the TUI has no idea.

## The TUI has no fact about panes

The status snapshot knows the roster, auto-mode, waits, and verdicts.
Nothing in it says whether an agent HAS a pane. The zellij worker
lists panes — to reconcile and to retitle — and keeps that knowledge
to itself; the loop learns only whether zellij answered (`reach`).
So the row cannot show it, and the menu could only offer the item
unconditionally and let a listing answer "already open" after the
fact (reopen-an-agents-pane-from-its-menu).

## Change

**Presence is a fact the worker reports, on its own clock.** A pane
closed by hand or a process that exits changes nothing under the
repo: no watcher event, no refresh, no roster message — and the
loop's idle timeout only repaints. So presence cannot ride on the
refresh batch; the last "present" would stand forever, exactly from
the moment the pane disappeared (codex on 63548a9). It is its own
input with a bounded trigger:

- The worker waits for messages with a timeout (`PRESENCE_PERIOD`,
  a few seconds) and probes on every timeout — one `list-panes
  --json --command` (measured 23 ms here; not on the cost gate's
  expensive list), projected to the set of this repo's labels with a
  LIVE pane: a non-exited pane running the byte-exact launch command,
  via `ZellijPane::runs`.
- The loop asks for an immediate probe (`WorkerMsg::Probe`) when the
  agent panel or an agent's page is entered, so the menu is right
  when it opens.
- A refresh batch probes too — it is listing anyway.

The result reaches the loop the way reach does — on the worker's
channel, drained without blocking before every paint — and the
worker wakes the loop (`Ev::Worker`) so a change repaints at once
without rebuilding repo state. Outside zellij, or when the listing
fails, the fact is unknown, which is not the same as absent.

Reports become one channel and one enum — reach, presence, reopen
outcomes — instead of a channel per kind.

**The mark says it.** `auto_mark` takes the agent's presence: `?`
(dim) when the agent has no live pane, `▶`/`⏸` as today when it has
one or when presence is unknown. Same fixed field width. A dead pane
and a missing pane look the same from the roster: neither is open.

**The menu knows.** `detail_actions(role, presence)` offers
`reopen pane` only when the agent has no live pane. Unknown presence
(not in zellij) hides it too — there is nothing to reopen into. The
page's cursor clamps when the list shrinks under it.

**Reopen brings back a dead pane as well as a missing one.** Today a
pane whose process exited is still that label's pane to the
reconciler — deliberately, so `remove` can close the corpse — which
makes `reopen` answer "already open" for an agent that is plainly
not. The targeted reopen now checks liveness: if every pane the label
has is exited, it closes them (`remove_all`, which prefers exited
copies), takes a fresh listing so `add` does not find the corpse by
identity, and adds. A live pane is still "already open".

**The reopened pane makes the holder decision.** Nothing new: the
pane runs `clank agent start <label> --repo <repo>`, and `agent
start` finds who holds the session before it composes the launch
(a-session-has-one-holder) — a claude background session is
attached, a pane live in another zellij session is refused with the
message naming it. The TUI does not duplicate that decision; it
launches through the command that makes it. Pinned by a test that
the reopen's `new-pane` argv is exactly that command.

## Tests

- The worker probes on its timeout with NO message queued (the loop
  driven with a short period and an empty channel), and on an
  explicit probe request; each probe is one listing; the set contains
  live labels only — an exited pane's label and another repo's pane
  are absent.
- The live set changes between two probes while no roster or refresh
  message arrives: the second report differs, and through it the row
  mark goes `▶` → `?` and the page GAINS its reopen row — the
  transition a hand-close causes, proven with nothing else moving.
  And the reverse, present again after a reopen: `?` → `▶` and the
  row gone. Both directions pinned, so a test cannot pass by leaving
  the row hidden throughout (codex on 580150a).
- `auto_mark`: `?` for a missing pane in both auto modes, the field
  width unchanged; `▶`/`⏸` for present and unknown.
- `detail_actions`: reopen offered when missing, for every role; not
  offered when present or unknown; the page cursor on the removed row
  lands on a real row.
- `reopen` on a label whose only pane is exited: closes it, lists
  again, adds, and reports by the read-back; on a label with a live
  pane: already open, nothing closed.
- The reopen's pane command is `clank agent start <label> --repo
  <repo>` byte for byte.

## Out of scope

- Restarting an exited pane in place (zellij's own Enter-to-rerun
  works there today; the reopen replaces it instead).
- Presence for agents in OTHER sessions — that is the holder scan's
  job at launch, not the roster's.
