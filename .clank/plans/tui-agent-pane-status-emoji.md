# tui-agent-pane-status-emoji

Show each agent's current status as an emoji prefix on its OWN zellij
pane name — `🔨 claude (master)` while master is working, `👀 codex
(reviewer)` while that reviewer is the one we're waiting on, a quiet
glyph (e.g. `💤`) when idle. Reuse the bar's existing emoji vocab
(the lamp glyphs — 🔨 working, 👀 reviewing, etc.).

This SUPERSEDES the dropped `tui-focus-waited-reviewer` (which stole
keyboard focus). Emoji-in-the-name is the non-invasive version of the
same intent: see who's active at a glance, without moving focus.

## The make-or-break (VERIFY FIRST — untested, and I couldn't test it)

zellij 0.44.3 has NO `rename-pane-by-id`, so the `status --tui` pane
CANNOT rename other agents' panes (confirmed during
tui-focus-waited-reviewer). So each pane must set its OWN title. The
natural lever is an OSC title escape (`printf '\033]2;<title>\007'`),
but two things are unverified — and I could NOT test them from the
agent harness (its shell has no controlling tty: `/dev/tty` is "device
not configured"):

1. **Does an OSC 2 title from inside a pane OVERRIDE the layout's
   `name="<label> (<role>)"`?** zellij may pin the layout/rename name
   and ignore the program title. If OSC is suppressed by the static
   name, this whole approach needs a different mechanism (e.g. drop
   the static `name=` from the agent panes so the program title shows
   through — but that loses the nice default names; or a zellij
   plugin). Test in a REAL agent pane first; do not build on the
   assumption.
2. **What process emits the OSC, and does it reach the pane's tty?**
   The emitter must (a) run IN the agent's pane (have its pty), (b)
   know the agent's role + current status. The clank Stop hook runs
   per-agent at turn boundaries and computes the wait state — but its
   stdout is consumed by the tool (JSON protocol), so it'd have to
   write OSC to `/dev/tty` directly. Verify the hook has a usable
   controlling tty in a real pane, and decide the trigger(s): Stop
   hook = turn END (reflects "what's next"); is there a turn-START
   signal to show 🔨 WHILE working, not just at hand-off?

Resolve BOTH before implementing — the feature is moot if a pane
can't override its layout name.

## If the mechanism works — design

- A pure `agent_status_emoji(role, <this agent's wait state>) ->
  &str` mapping, reusing the bar's glyphs (master working → 🔨,
  the awaited reviewer → 👀, idle/waiting → 💤, blocked → 🙋, …).
  Per-AGENT (not the whole-repo bar emoji): it answers "what is THIS
  agent doing", derived from role + the snapshot.
- The emitter (Stop hook and/or agent-start) writes `\033]2;<emoji>
  <label> (<role>)\033\\` to the pane's tty when this agent's status
  changes. Reuse a shared `<label> (<role>)` title helper
  (open_zellij.rs already builds it in `push_agent_pane`) so the
  emitted title matches the layout's — addresses the duplicated-
  literal smell ruthless flagged.
- Gated on `$ZELLIJ`. Each agent only ever titles ITS OWN pane (no
  rename-pane-by-id needed).

## Testing

- Pure: `agent_status_emoji(role, state)` across master-working,
  reviewer-awaited, idle, blocked — unit-tested.
- The OSC emission + the override behavior are verified manually in a
  real pane (per the make-or-break above); not unit-testable (no tty
  in the test/agent-harness env).

## Non-goals

- Focus changes of any kind (that was the dropped approach).
- Renaming OTHER agents' panes from one pane (no rename-pane-by-id).
- The whole-repo tab emoji (already shipped: tui-tab-mirror-bar-emoji)
  — this is the per-AGENT-pane complement.
