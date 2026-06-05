# document-clank-open-zellij-new-tab

`clank open zellij` already does the right thing whether
you're inside a zellij session or not — `zellij --layout
<path>` auto-detects and adds a new tab when inside a
session, starts a new session when outside (per `zellij
--help`). But this isn't documented in `clank open zellij
--help`, so users might not realize the command is safe to
run from inside their existing session.

## Goal

Two doc changes to `crates/cli/src/cli/mod.rs`:

1. **Fix the stale variant docstring** on `OpenCmd::Zellij`.
   Currently reads "Spawn the tab via `zellij action
   new-tab`" — that command path was discussed in early
   drafts but never implemented (the actual spawn is
   `zellij --layout <path>`, verified at `open_zellij.rs:37`).
   This is what `clank open --help` displays for the
   subcommand summary, so users seeing this surface
   get a wrong description.
2. **Document the new-tab-vs-new-session auto-detect** at
   `OpenZellijArgs`'s struct level so it appears in
   `clank open zellij --help`'s long-about.

Verified before promotion (2026-06-06): ran `clank open
zellij --help` and confirmed today's output starts with
the stale "Spawn the tab via `zellij action new-tab`"
text. The variant doc comment at `mod.rs:194-197` is the
source.

## Approach

In `crates/cli/src/cli/mod.rs`:

1. Replace the stale `OpenCmd::Zellij` variant docstring
   (`:194-197`) with an accurate short summary, e.g.:
   `Auto-generate a zellij layout (KDL) for master +
   reviewer panes and spawn it via \`zellij --layout\` —
   opens as a new tab when run inside an existing session,
   starts a new session otherwise. Use \`--print\` to
   emit the composed KDL on stdout + the would-be-spawned
   argv on stderr instead of shelling out.`
2. Add a struct-level docstring on `OpenZellijArgs`
   (`:215`) explicitly noting the auto-detect contract:
   ```
   /// Inside an existing zellij session, the layout opens
   /// as a new tab (no new session is spawned). Outside
   /// any session, it starts a new session. This is
   /// zellij's `--layout` behavior — no extra detection
   /// in clank.
   ```
   This becomes the `long_about` for `clank open zellij
   --help`.

That's the entire scope. No code changes; just docstrings.

## Tests

- One integration test in
  `crates/cli/tests/open_zellij_integration.rs`:
  `clank_open_zellij_help_documents_auto_detect`. Spawn
  `clank open zellij --help`; assert stdout contains both
  `new tab` and `new session` (contains-checks, not exact
  wording, so future tweaks can tighten without breaking
  the test).
- Defend against regression to the stale `action new-tab`
  text: assert `clank open --help` does NOT contain
  `action new-tab` (the wrong description). The current
  spawn is `zellij --layout`, not `zellij action new-tab`.

## Out of scope

- Verifying the runtime behavior (zellij does this; we
  don't need to re-test their feature).
- Any code changes beyond the two docstrings.
- Restructuring `OpenZellijArgs`'s per-field doc comments.

## Acceptance

- `clank open --help` summary line for the `zellij`
  subcommand uses an accurate description (no
  `action new-tab` claim).
- `clank open zellij --help` long-about mentions both
  "new tab" (inside session) and "new session" (outside)
  behaviors.
- The doc-presence integration test passes; the
  regression-against-`action new-tab` assertion passes.
- `cargo test --workspace` passes.

## Why

I previously drafted `open-zellij-auto-detects-inside-session`
on the wrong premise that today's `clank open zellij` would
fail or break the user's current session when running inside
zellij. Lloyd 2026-06-06 caught it — zellij's own
`--layout` flag already handles both cases. The auto-detect
is shipped; we just need to tell users.
