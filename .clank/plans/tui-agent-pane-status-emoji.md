# tui-agent-pane-status-emoji

Show each agent's current status as an emoji prefix on its OWN zellij
pane name — `🔨 claude (master)` while master is working, `👀 codex
(reviewer)` while that reviewer is acting, a quiet glyph (`💤`) when
idle. Reuse the bar's lamp-glyph vocab.

This SUPERSEDES the dropped `tui-focus-waited-reviewer` (which stole
keyboard focus). Emoji-in-the-name is the non-invasive version: see
who's active at a glance, no focus movement.

## Mechanism — RESOLVED (verified live, zellij 0.44.3)

`zellij action rename-pane "<name>"` renames the INVOKING pane — the
one identified by `$ZELLIJ_PANE_ID`, NOT the globally-focused pane.
Verified: running it from the master pane (`ZELLIJ_PANE_ID=0`) renamed
`terminal_0`, leaving the others untouched. So:

- Each agent renames ITS OWN pane by running `zellij action
  rename-pane` from within it. No `rename-pane-by-id` needed, no
  focus-stealing, no status-pane-renames-others.
- This goes to the zellij SERVER directly, so it works even though
  the agent tool (claude/codex) renders the terminal — which is why
  the earlier OSC-title idea failed (claude printed the escape as
  literal text; the server never saw it). OSC is abandoned.

## Driver — the per-agent Stop hook

The Stop hook already runs per-agent IN that agent's pane (inherits
`$ZELLIJ_PANE_ID`), resolves the agent's role + label, and computes
the wfw outcome — exactly the inputs needed. Right before it returns,
set the pane title best-effort:

- outcome = has work (agent is about to act) → role master → `🔨`,
  role reviewer → `👀`
- outcome = no work (Silent / idle / waiting) → `💤`

Setting it at the moment work is assigned means the glyph reflects the
turn the agent is ABOUT to run (correct timing — the reviewer that's
handed a review shows 👀 as it reviews, not after). `clank agent
start` may also set an initial title. Gated on `$ZELLIJ`; best-effort
(a rename failure must never disturb the hook's outcome).

## Design

- Pure `agent_status_emoji(role, has_work) -> &'static str` — the
  per-AGENT mapping above (vs the whole-repo `bar_emoji`). Unit-tested.
- A shared `agent_pane_title(label, role)` (or `…_with_emoji`) helper
  used by BOTH `push_agent_pane` (open_zellij.rs, the layout name) and
  the Stop-hook renamer, so the emitted title always matches the
  layout's base name — removes the duplicated `"<label> (<role>)"`
  literal (the drift smell ruthless flagged on
  tui-tab-mirror-bar-emoji).
- The renamer shells `zellij action rename-pane` best-effort, only
  when `$ZELLIJ` is set.

## Testing

- Pure: `agent_status_emoji(role, has_work)` across master-working,
  reviewer-acting, idle — unit-tested.
- Pure: `agent_pane_title` shared by layout + renamer (one source).
- The `rename-pane` side effect is shelled best-effort and verified
  manually in a real pane (the mechanism is already live-confirmed);
  not unit-tested (no zellij in the test env).

## Non-goals

- Focus changes of any kind (the dropped approach).
- Renaming OTHER agents' panes from one pane (unnecessary — each
  self-renames; and there's no `rename-pane-by-id` anyway).
- The whole-repo tab emoji (shipped: tui-tab-mirror-bar-emoji) — this
  is the per-AGENT-pane complement.
