# clank-open-zellij-context — bare `clank open` does the zellij thing

Mini plan (lloyd 2026-06-10): `clank open` currently REQUIRES a
subcommand (`dry` | `zellij`). Make the subcommand optional —
bare `clank open` runs the zellij opener, which already
auto-detects its context via `$ZELLIJ`:

- inside a zellij pane → new tab in the current session
- outside → attach-or-create the deterministic
  `clank-<basename>` session (zellij-session-dedup)

So `clank open` becomes THE one-word "get me my agent panes"
verb in every context, and `clank open dry` stays the explicit
path classifier.

## Surfaces

- `cli/mod.rs` `OpenArgs`: `command: OpenCmd` →
  `Option<OpenCmd>`; the zellij-specific flags (`--print`,
  `--repo`) need to be reachable on the bare form — either lift
  them onto OpenArgs (delegating when no subcommand) or
  `args_conflicts_with_subcommands` + flatten. Pick whichever
  keeps `clank open --print` working.
- `main.rs` dispatch: `None => open_zellij::run(...)` with the
  lifted args.
- Help text: `clank open` one-liner says it opens the zellij
  workspace; `dry` documented as the classifier subcommand.

## Pattern note for `clank fork`

This establishes the convention "the bare verb does the
context-appropriate zellij thing." When `clank-fork-worktree-
sessions` lands, its `--open zellij` sugar should follow suit:
inside a zellij session, `clank fork <name>` can default to
opening the tab (with `--no-open` as the opt-out) — recorded
here so the fork sizing inherits the decision consciously.

## Verification (in-process)

- clap shape: bare `clank open` parses and routes to the zellij
  runner; `clank open dry <path>` unchanged; `clank open
  --print` works on the bare form (Cli::try_parse_from tests).
- No behavior change inside open_zellij itself (context
  detection already lives there and is tested).

## Status

Mini stub — queued lloyd 2026-06-10. Land BEFORE
clank-fork-worktree-sessions (the fork plan inherits the
bare-verb convention).
