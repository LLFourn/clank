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

## Per-agent emoji (pure, from the snapshot) — ONE shared predicate

codex 07f8db1: a bespoke `master_working` that only checks plan
`waiting_on` master variants DRIFTS — it misses the other states where
the bar/hook wake master: queue-promote, and PR-review master turns
(round-0 drafting, refining/submitting with no missing reviewers). The
fix is NOT to re-enumerate; it's to share the ONE predicate
`state_color` already uses to paint "master working" green/cyan.

- Factor `master_is_active(snap) -> bool` out of `state_color`, with
  the SAME BRANCH PRECEDENCE it uses today (codex fbdb27b — NOT a flat
  OR; plans dominate, so a queue/PR-ready state must NOT mark master
  active while a plan still awaits reviewers):
  1. blocked or idle → `false`
  2. else if any plans exist → `true` iff some plan is
     `MasterTo{Revise,Continue,Commit,Finalize}` (queue/PR ignored —
     plans take precedence)
  3. else if PR reviews exist → `true` iff NO PR has missing reviewers
  4. else if the queue is non-empty → `true` (promote)
  5. else → `false`

  REFACTOR `state_color` to call this (green/cyan when true — cyan is
  the case-4 queue branch, green otherwise; yellow when false), so the
  bar hue and the pane emoji are one source and cannot disagree.
- `awaited_reviewers(snap)` = union of every `missing` set
  (`ReviewerApprovalsMissing` + `GateReviewersMissing` across plans,
  plus each PR review's `missing_reviewers`).
- `agent_status_emoji(snap, label, role)`:
  - Reviewer → `👀` if `label ∈ awaited_reviewers` else `💤`
  - Master → `🔨` if `master_is_active` else `💤`

Explicit glyph decision (codex's ask): the pane indicator is COARSE —
ALL master-work states (build / revise / commit / finalize / promote /
PR draft / refine / submit) show the single `🔨`; the bar keeps the
fine-grained glyph (📋/🏁/🔨…). The pane answers "is this agent
working", not "exactly what". Multi-plan naturally supports BOTH a
`🔨` master and `👀` reviewers at once (master active on one plan,
reviewers awaited on another) — unlike the single-hue bar.

## Design / testing

- Pure + unit-tested: `master_is_active` — incl. the PRECEDENCE
  regression codex fbdb27b named: a plan awaiting reviewers + a
  non-empty queue is NOT master-active (plans dominate; master `💤`,
  reviewers `👀`), AND a clean PR / queue-only IS master-active.
  `awaited_reviewers`,
  `agent_status_emoji` (master-active, reviewer-awaited, idle), and a
  `parse_agent_panes(list_panes_stdout) -> [(id,label,role)]` parser
  pinned to the live 0.44.3 fixture (reuse `strip_leading_emoji` to
  drop a prior glyph so re-applies are stable). `state_color` keeps its
  existing tests (now routed through the shared predicate).
- A `PaneStatus` tracker beside `TabIndicator`: dedups per pane id,
  `$ZELLIJ`-gated, shells `rename-pane` best-effort.
- The rename side effect + list-panes shape are live-verified; not
  unit-tested (no zellij in the test env).

## Non-goals

- Focus changes of any kind (the dropped approach).
- The Stop hook doing any of this (kept clean).
- The whole-repo tab emoji (shipped: tui-tab-mirror-bar-emoji) — this
  is the per-AGENT-pane complement.
