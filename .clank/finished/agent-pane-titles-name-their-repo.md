# agent-pane-titles-name-their-repo
# Agent pane titles name their repo

## Problem

One zellij session holds many repos. `clank open` run inside a session adds
the repo as a tab there, so `clank-life` holds life, clankstack,
protonmailmcp, rotate-wallpapers-mac, penlock-experiment and hl-harness.
Each has its own `codex` reviewer.

Zellij's inbox (F9) lists panes as `<session> <pane title>`, without the tab.
Our agent pane title is the label alone (`open_zellij.rs`
`agent_pane_title`), so the inbox shows six identical `clank-life codex`
rows, and none of them says which repo the agent works in.

## Change

An agent's pane title names its repo: `<glyph> <label> · <repo>`, for example
`💤 codex · clankstack`.

- `<repo>` is the base name `clank open` gives the repo's tab before any
  disambiguation: the repo's directory, or the worktree's directory for a
  worktree. Take it from that same function, so the tab and the pane title
  cannot drift apart.
- `agent_pane_title` stays the one source of the format. It takes the repo,
  and every writer passes it: the layout, `new-pane`, `clank as` and the
  TUI's glyph retitle.
- The title stays the pane's identity (`resolve_title`). Reading a title
  strips a ` · <repo>` suffix only when `<repo>` is this workspace's name,
  so the set of readings stays closed. Bare `<label>` and
  `<glyph> <label>` titles still resolve, so panes opened by older builds
  are found and then retitled. They are not orphaned.

## Must check first

Today most agent panes carry a bare `codex` or `claude` title, without the
glyph. Only some tabs (hl-harness, and the newest s3-provisioning panes) show
`💤 codex`, so the glyph retitler is not reaching most panes. The new name
rides on that retitler. Find out why it skips those panes and fix that cause
as part of this plan, otherwise most panes keep their old names. If the cause
turns out to be large, stop and report it rather than widening this plan.

## Out of scope

- Session names. A session holds several repos, so no session name can be
  right. The pane title carries the repo instead.
- The empty name on the protonmailmcp tab.
- How zellij's attention status treats codex while it sits in the Stop hook.

## Acceptance

- After a TUI pass, every live agent pane in a workspace is titled
  `<glyph> <label> · <repo>`, including panes created by older builds.
- Identity: a unit test per reading (bare label, glyph plus label, new
  suffixed form, old `<label> (<role>)`). A suffix naming a different repo
  must not resolve.
- Every writer passes the repo: a test fails if any writer drops it.
- Tests for the retitler-skip cause, built from the mechanism found.
