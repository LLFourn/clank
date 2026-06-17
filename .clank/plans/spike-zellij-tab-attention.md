# spike-zellij-tab-attention

SPIKE (exploratory — deliverable is a recommendation + thin
prototype, NOT production code). Question: can a clank watcher edit
its zellij tab's name to signal state — e.g. `💤` when idle, `❌`
when blocked — and does it tear down cleanly?

## Goal

When clank is running in a zellij tab, the tab name reflects the
team's attention state so a glance at the tab bar tells you which
worktree needs you:
- **Blocked** (a block awaits a human / `clank block`): prepend a
  loud glyph, e.g. `❌` (user's "big X").
- **Idle** (no active plan, nothing pending): prepend `💤` (user's
  "sleeping ZZZ").
- **Active** (reviewers/master working): no glyph — the bare name.

The state classification ALREADY exists: `status_tui::state_color`
distinguishes blocked (red) / idle (dim) / active. Extract a pure
`attention_state(&StatusSnapshot) -> Blocked | Idle | Active` from
that logic — this is the one piece worth a real unit test.

## The mechanism (verified against zellij 0.44.3)

- No first-class "tab needs attention" flag exists. The supported
  primitive is `zellij action rename-tab-by-id <ID> <NAME>` — it
  renames a SPECIFIC, non-focused tab by stable id. Clear by
  renaming back (or `undo-rename-tab`).
- Tab id discovery: `zellij action current-tab-info` (from inside
  the tab) or `list-tabs` / `query-tab-names`.
- Glyph hygiene: strip/replace any existing leading glyph so they
  never stack, and restore the ORIGINAL name on return to Active.

## THE risk to resolve first: lifecycle / no leaks

A watcher-per-tab that outlives its tab is the same class of bug as
the leaked zellij servers that congested the machine. The spike
MUST prove clean teardown before anything else. Compare two
approaches:

**A. Fold into the existing `clank status --tui` pane.** The
built-in layout already runs one `status --tui` process per clank
tab; it's already inside zellij ($ZELLIJ set), already event-driven
on the same state, and already dies when the tab/pane closes — so
the lifecycle is solved for free. Extend it: on an
`attention_state` transition, shell `zellij action current-tab-info`
+ `rename-tab-by-id`. Cost: only works for layouts that include the
status pane (the built-in does; arbitrary user templates may not).

**B. A forked detached watcher** spawned by `clank open zellij` (the
user's original framing). Works regardless of layout, but OWNS the
teardown problem: it must reliably exit when its tab/session is
gone (poll `list-tabs`/`list-sessions` and self-terminate; handle
the daemon hanging / the session dying mid-poll). Higher leak risk —
the spike must demonstrate it never orphans.

Recommendation bias: A is likely better precisely because it dodges
the leak risk, but the spike should confirm A can reach the tab id
and rename without disrupting the TUI render, and quantify B's
teardown cost before deciding.

## Open questions for the spike to answer

1. From inside a tab, does `current-tab-info` give a stable id that
   `rename-tab-by-id` accepts? (round-trip a rename live.)
2. Does renaming a tab perturb the running `status --tui` pane
   (redraw glitches, focus changes)? Run it and watch.
3. Teardown: close the tab / kill the session — does the chosen
   approach leave ANY orphan process? (the make-or-break check.)
4. Re-run / attach: on `clank open zellij` that ATTACHES (not
   creates), is a second watcher spawned (B), or is the existing
   TUI pane already covering it (A)? Avoid double watchers.
5. Glyph rendering in the zellij tab bar (emoji width, the
   compact-bar vs tab-bar plugins) — do `💤`/`❌` display cleanly?

## Deliverable

- A short written recommendation (A vs B, with the teardown
  evidence) appended to the plan.
- A minimal prototype proving the round-trip (state change → tab
  rename → restore) and the teardown, verified MANUALLY by running
  zellij and watching the tab bar (consistent with the
  no-binary-spawning-TESTS rule; only the pure `attention_state`
  classifier gets an automated test).

## Non-goals (defer to the follow-up implementation plan)

- Config knobs (custom glyphs, opt-out), per-template handling,
  Windows.
- Pane-level (vs tab-level) indicators.
- Any rich/colored indicator needing a custom zellij WASM plugin.
