# tui-tab-mirror-bar-emoji

The `clank status --tui` bar already leads with an emoji that encodes
what's happening (👀 reviewing, 🔨 master working, 🏁 finished, 🙋
blocked, 🔍 gate review, 🔀 multi-plan, …). Simplest possible
attention indicator: mirror THAT emoji into the zellij tab name, so
the tab bar shows it at a glance. No new state→glyph mapping — reuse
whatever the bar is already showing.

This is the implementation follow-up to [[spike-zellij-tab-attention]]
(Approach A: fold into the `status --tui` pane, which already runs per
tab and dies with it — no extra process, no orphan risk; the
`rename-tab-by-id` mechanism was live-confirmed in the spike). It
SUPERSEDES the spike's 💤/❌ 3-state idea — the bar emoji is richer and
already computed.

## Design

- **Single source of truth for the emoji.** Today the bar emoji is
  chosen inline across `bar_text` / `pr_bar_text` / `emoji_of` (the
  plan, PR, blocked, idle, multi-plan branches). Factor it into one
  `bar_emoji(&StatusSnapshot) -> String` that `bar_text` also uses,
  so the tab and the bar can never disagree.
- **Fold into the TUI loop** (`run_tui`, status_tui.rs), gated on
  `$ZELLIJ`:
  - At startup: read the tab once via `zellij action
    current-tab-info` (id + the original name); cache both.
  - On each frame, compute `bar_emoji(snap)`; if it CHANGED since the
    last applied value, `zellij action rename-tab-by-id <id>
    "<emoji> <original_name>"`. Best-effort (ignore errors), only on
    change (not every frame).
  - Always rebuild from the cached ORIGINAL name + current emoji, so
    emojis never stack and a manual base name isn't clobbered.
  - On clean exit (the existing SIGINT handler): restore the original
    tab name (strip our emoji) — spike's glyph-hygiene note.
- Non-zellij (`$ZELLIJ` unset) or `current-tab-info` failure: do
  nothing, render exactly as today.

## Open question (cheap to settle)

What does the bar show when IDLE? Confirm there is a sensible idle
emoji (or decide one) so the tab isn't left with a stale "working"
glyph when nothing's happening. `attention_state` (already built) can
inform the idle case if the bar lacks a distinct one.

## Testing

- Pure: `bar_emoji(snap)` returns the expected emoji for each state
  (plan-active, reviewers, master-turn, blocked, idle, multi-plan,
  PR-review) — and `bar_text` still starts with that same emoji
  (single-source assertion).
- The rename side effect is the spike-confirmed `zellij action`
  (verified live there); the loop wiring is thin and gated, exercised
  manually (no clank-binary spawning in tests). The emoji-change
  dedup logic (only rename on change) can be unit-tested as a pure
  transition check.

## Non-goals

- Custom/config glyphs, opt-out (later).
- Per-template handling: only tabs whose layout includes a `status
  --tui` pane get the indicator (the built-in does) — documented.
- Pane-level indicators; colored/blinking (needs a plugin).
