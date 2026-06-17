# tui-agent-pane-status-emoji

Show each agent's current status as an emoji prefix on its OWN zellij
pane name — `🔨 claude (master)` while master is working, `👀 codex
(reviewer)` while that reviewer is awaited, `💤` when idle. Reuse the
bar's lamp-glyph vocab.

This SUPERSEDES the dropped `tui-focus-waited-reviewer` (which stole
keyboard focus). Emoji-in-the-name is the non-invasive version: see
who's active at a glance, no focus movement.

## Mechanism — RESOLVED (verified live, zellij 0.44.3)

`zellij action rename-pane -p/--pane-id <ID> "<name>"` renames ANY
pane by id (not just the focused/invoking one). Verified live:
from the master pane, `rename-pane --pane-id terminal_1 "👀 codex
(reviewer)"` renamed codex's pane and restored cleanly. Pane ids +
titles come from `zellij action list-panes` (`PANE_ID  TYPE  TITLE`,
one per line). This goes to the zellij server directly, so it's
unaffected by the agent tool's terminal rendering (the earlier OSC
idea failed because claude printed the escape as literal text — OSC
abandoned).

## Driver — the `status --tui` pane (central), NOT the Stop hook

The status pane already holds the whole-repo snapshot every frame and
already renames the tab (`TabIndicator`). It can see and rename every
agent pane by id, so it owns this too. The Stop hook stays clean — no
pane-renaming on the critical path (it's exactly the kind of side
concern that does not belong in `stop_hook.rs`).

Each frame, gated on `$ZELLIJ`:
1. `list-panes` → for each agent pane (title `"<label> (master)"` /
   `"<label> (reviewer)"`, after stripping any existing emoji),
   recover `(pane_id, label, role)`.
2. Compute that agent's emoji from the snapshot.
3. Rename via `rename-pane --pane-id <id> "<emoji> <label> (<role>)"`
   — only when the desired title CHANGED for that pane (dedup per
   pane id). Best-effort; a rename failure never disturbs the render
   loop.

## Per-agent emoji (pure, from the snapshot)

- `awaited_reviewers(snap)` = union of every `missing` set
  (`ReviewerApprovalsMissing` + `GateReviewersMissing` across plans,
  plus each PR review's `missing_reviewers`).
- `master_working(snap)` = any plan whose `waiting_on` is a master
  action (`MasterToRevise | MasterToContinue | MasterToFinalize |
  MasterToCommit`).
- `agent_status_emoji(snap, label, role)`:
  - Reviewer → `👀` if `label ∈ awaited_reviewers` else `💤`
  - Master → `🔨` if `master_working` else `💤`

  (Multiple reviewers can be `👀` at once; a `Blocked` plan leaves
  agents `💤` — the human-action signal lives on the bar/tab as 🙋.)

## Design / testing

- Pure + unit-tested: `awaited_reviewers`, `master_working`,
  `agent_status_emoji` (master-working, reviewer-awaited, idle), and a
  `parse_agent_panes(list_panes_stdout) -> [(id,label,role)]` parser
  pinned to the live 0.44.3 fixture (reuse `strip_leading_emoji` to
  drop a prior glyph so re-applies are stable).
- A `PaneStatus` tracker beside `TabIndicator`: dedups per pane id,
  `$ZELLIJ`-gated, shells `rename-pane` best-effort.
- The rename side effect + list-panes shape are live-verified; not
  unit-tested (no zellij in the test env).

## Non-goals

- Focus changes of any kind (the dropped approach).
- The Stop hook doing any of this (kept clean).
- The whole-repo tab emoji (shipped: tui-tab-mirror-bar-emoji) — this
  is the per-AGENT-pane complement.
