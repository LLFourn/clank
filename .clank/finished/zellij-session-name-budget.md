# zellij-session-name-budget
# `clank open zellij` fits the session name into zellij's socket budget

## Bug (lloyd, live at ~/src/fswt/frostsnap_nostr-taipei)

`clank open zellij` fails at zellij's arg parse:

    error: Invalid value "clank-frostsnap_nostr-taipei" for '--session
    <SESSION>': session name must be less than 0 characters

Diagnosed on-machine: zellij validates the session name against a UNIX
socket-path budget — socket = `$TMPDIR/zellij-<uid>/<version>/<name>`, and
on macOS the long `$TMPDIR` (49 chars) leaves a budget of exactly **24
chars** for the name (empirically: 24 OK, 25 refused, probed via
`zellij --session <name> setup --check`, which validates at parse time
without spawning a server). `clank-<basename>` for this repo is 28 chars.
Short-named repos (`clank-clank`, 11) never hit it. The "less than 0"
number is zellij's own display bug; the refusal is real.

## Fix

Cap the generated session name DETERMINISTICALLY in the one place it is
built (`open_zellij.rs`'s `clank-<basename>` builder):

- If `clank-<basename>` fits a conservative cap (24 chars — the macOS
  standard-TMPDIR budget; Linux runtime dirs are shorter so 24 is safe
  everywhere reasonable), use it unchanged — existing sessions keep their
  names.
- Else truncate the basename and append a short hash of the FULL basename
  (e.g. `clank-frostsnap_no-4fa2`) so distinct long repos never collide
  and the name stays deterministic per repo — reconciliation (find the
  session, add missing tabs) depends on determinism.
- ONE builder used by every session-name consumer (open/attach/reconcile/
  fork tab targeting); no other call site constructs the name.

Rejected: setting `ZELLIJ_SOCKET_DIR` to a short path — sessions would
land in a different socket namespace, invisible to the user's own
`zellij ls`/attach; breaks interop for a corner case.

Optional (note in plan, not required): report the "less than 0
characters" display bug upstream to zellij.

## Tests (pure)

- The name builder: short basename → unchanged (back-compat with existing
  sessions); long basename → ≤ 24 chars, deterministic, distinct for
  distinct basenames sharing a 17-char prefix; always starts `clank-`.

## Acceptance

- `clank open zellij` works in a repo with a 22+ char basename; existing
  short-named sessions are unaffected; clippy/fmt/suites green.
